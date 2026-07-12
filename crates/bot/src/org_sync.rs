//! Org reconciliation loop (§11.2) — hourly + on config change.
//!
//! **Registry-gated discovery (§2):** a project exists only when the GitHub
//! App is installed on the org AND the org is listed in the root registry.
//! Installed-but-unlisted does nothing; listed-but-uninstalled logs "pending".
//!
//! For each discovered org:
//!   1. ensure the `ops` repo exists (created with a starter scaffold)
//!   2. create missing app repos from `repo-templates/<template>/` in the
//!      platform repo (GHA workflow, .gitignore, …)
//!   3. archive app repos removed from config — never delete (§2)
//!   4. enforce branch protection: ops `env/production` requires an admin
//!      review (the production gate, §9), app `main` requires the build check
//!   5. sync org teams (`admins`, `developers`) + membership from project.yaml
//!
//! No per-repo webhooks are needed: the GitHub App's event subscription
//! covers every repo in every installed org.

use anyhow::{Context, Result};
use majnet_common::platform::ProjectsFile;
use majnet_common::project::{ProjectConfig, Role};
use serde_json::json;
use std::collections::BTreeMap;

use crate::AppState;

pub async fn sync_all(state: &AppState) -> Result<()> {
    let (_, platform_tar) =
        crate::proxy::fetch_snapshot(state, &state.config.root_org, "platform", "main").await?;
    let platform = majnet_common::tarball::untar(&platform_tar)?;
    let projects = ProjectsFile::parse(
        platform
            .get("projects.yaml")
            .context("platform repo has no projects.yaml")?,
    )?;

    let mut synced: Vec<(String, ProjectConfig)> = Vec::new();
    for entry in &projects.projects {
        // The discovery gate: registry entry ∧ App installation.
        if state.github.org_client(&entry.org).await.is_err() {
            tracing::warn!(
                org = entry.org,
                project = entry.name,
                "registered but App not installed — pending"
            );
            continue;
        }
        match sync_org(state, &entry.org, &platform).await {
            Ok(Some(config)) => synced.push((entry.name.clone(), config)),
            Ok(None) => {}
            Err(e) => {
                tracing::error!(org = entry.org, error = format!("{e:#}"), "org sync failed");
                state
                    .store
                    .log_event("org-sync", Some(&entry.org), &format!("FAILED: {e:#}"))?;
            }
        }
    }

    // Membership drives the access network too (§5): one identity, two syncs.
    if let Some(people_yaml) = platform.get("people.yaml") {
        let people = majnet_common::platform::PeopleFile::parse(people_yaml)?;
        crate::tailscale::sync_acl(state, &people, &synced).await?;
    }
    Ok(())
}

pub async fn sync_org(
    state: &AppState,
    org: &str,
    platform: &BTreeMap<String, Vec<u8>>,
) -> Result<Option<ProjectConfig>> {
    let client = state.github.org_client(org).await?;

    ensure_ops_repo(state, &client, org).await?;

    let (_, ops_tar) = crate::proxy::fetch_snapshot(state, org, "ops", "main").await?;
    let ops = majnet_common::tarball::untar(&ops_tar)?;
    let Some(project_yaml) = ops.get("project.yaml") else {
        tracing::info!(
            org,
            "ops repo has no project.yaml yet — nothing to reconcile"
        );
        return Ok(None);
    };
    let project: ProjectConfig =
        serde_yaml::from_slice(project_yaml).context("parsing project.yaml")?;

    // App repos: create from template, archive removed.
    let existing = list_org_repos(&client, org).await?;
    for app in &project.apps {
        if !existing.contains_key(&app.name) {
            create_repo_from_template(&client, org, &app.name, &app.template, platform).await?;
            state.store.log_event(
                "repo-created",
                Some(org),
                &format!("{} (template {})", app.name, app.template),
            )?;
        }
        protect_app_main(&client, org, &app.name).await?;
    }
    for (repo, archived) in &existing {
        // Never touch the ops repo, the platform config repo (only present
        // when the root org is mistakenly registered as a project — archiving
        // it takes the whole platform read-only), or already-archived repos.
        if repo == "ops" || repo == "platform" || *archived {
            continue;
        }
        if !project.apps.iter().any(|a| &a.name == repo) {
            // Archival is the safe terminal state — never delete (§2).
            let _: serde_json::Value = client
                .patch(
                    format!("/repos/{org}/{repo}"),
                    Some(&json!({ "archived": true })),
                )
                .await?;
            tracing::info!(org, repo, "archived (removed from project.yaml)");
            state.store.log_event("repo-archived", Some(org), repo)?;
        }
    }

    protect_ops_production(&client, org).await?;
    sync_teams(&client, org, &project).await?;

    state.store.log_event("org-sync", Some(org), "ok")?;
    Ok(Some(project))
}

async fn ensure_ops_repo(state: &AppState, client: &octocrab::Octocrab, org: &str) -> Result<()> {
    // Attempt creation and tolerate 422 ("already exists"). A 422 means the
    // repo is already there and scaffolded, so we're done; only a freshly
    // created repo falls through to scaffolding. We avoid a read-then-create
    // guard because an installation token can 404 a GET on a repo it just
    // created (see the same fix in platform_api::do_seed).
    tracing::info!(org, "ensuring ops repo exists");
    match client
        .post(
            format!("/orgs/{org}/repos"),
            Some(&json!({
                "name": "ops",
                "description": "MajNet project config — managed by the platform",
                "private": true,
                "auto_init": false,
                // Merging a render PR is the deploy trigger (§16), and the bot
                // merges with `merge_method: "merge"`. Orgs that default to
                // rebase-only would 405 that merge, so ensure merge commits are
                // allowed on the ops repo we own.
                "allow_merge_commit": true,
            })),
        )
        .await
    {
        Ok::<serde_json::Value, _>(_) => tracing::info!(org, "created ops repo"),
        Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 422 => return Ok(()),
        Err(e) => return Err(e).context("creating ops repo"),
    }

    let scaffold = BTreeMap::from([
        (
            "project.yaml".to_string(),
            format!("name: {org}\nmembers: []\napps: []\n"),
        ),
        (
            ".sops.yaml".to_string(),
            "# SOPS recipient rules — add the platform class keys and project\n\
             # admins' age keys per secrets.<class>.yaml (design doc §14).\ncreation_rules: []\n"
                .to_string(),
        ),
        (
            "README.md".to_string(),
            format!(
                "# {org} — ops\n\nProject config for the MajNet platform. Edit `main`; the bot renders\n\
                 `env/*` branches via render PRs — merging a render PR is the deploy trigger.\n"
            ),
        ),
    ]);
    let repo = format!("/repos/{org}/ops");
    // The Git Data API can't write to a zero-commit repo (409 "empty"), so
    // initialize the freshly created repo via the Contents API first, then lay
    // the scaffold on top and force-update main (same approach as
    // platform_api::do_seed). The placeholder is dropped by the scaffold tree.
    client
        .repos(org, "ops")
        .create_file(
            ".majnet-init",
            "chore: initialize ops repo",
            "placeholder — replaced by the scaffold commit\n",
        )
        .send()
        .await
        .context("initializing empty ops repo")?;
    let parent = crate::git::get_branch_head(client, &repo, "main")
        .await?
        .context("ops main missing after initialization")?;
    let tree = crate::git::create_tree(client, &repo, &scaffold).await?;
    let commit = crate::git::create_commit(
        client,
        &repo,
        &tree,
        &[&parent],
        "chore: initial ops scaffold",
    )
    .await?;
    crate::git::force_update_ref(client, &repo, "main", &commit).await?;
    state.store.log_event("repo-created", Some(org), "ops")?;
    Ok(())
}

/// Materialize an app repo from `repo-templates/<template>/` in the platform
/// repo (§10). Template placeholders: `{{app}}` and `{{org}}`.
async fn create_repo_from_template(
    client: &octocrab::Octocrab,
    org: &str,
    app: &str,
    template: &str,
    platform: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let prefix = format!("repo-templates/{template}/");
    let files: BTreeMap<String, String> = platform
        .iter()
        .filter_map(|(path, content)| {
            let rel = path.strip_prefix(&prefix)?;
            let text = String::from_utf8(content.clone()).ok()?;
            Some((
                rel.to_string(),
                text.replace("{{app}}", app).replace("{{org}}", org),
            ))
        })
        .collect();
    anyhow::ensure!(
        !files.is_empty(),
        "template '{template}' not found in platform repo (repo-templates/{template}/)"
    );

    tracing::info!(org, app, template, "creating app repo from template");
    let _: serde_json::Value = client
        .post(
            format!("/orgs/{org}/repos"),
            Some(&json!({ "name": app, "private": true, "auto_init": false })),
        )
        .await
        .with_context(|| format!("creating repo {app}"))?;

    let repo = format!("/repos/{org}/{app}");
    let tree = crate::git::create_tree(client, &repo, &files).await?;
    let commit = crate::git::create_commit(
        client,
        &repo,
        &tree,
        &[],
        &format!("chore: scaffold from template {template}"),
    )
    .await?;
    crate::git::create_ref(client, &repo, "main", &commit).await?;
    Ok(())
}

/// The production gate (§9): merging into env/production requires an
/// approving review. Even a compromised dashboard can't skip this.
async fn protect_ops_production(client: &octocrab::Octocrab, org: &str) -> Result<()> {
    if crate::git::get_branch_head(client, &format!("/repos/{org}/ops"), "env/production")
        .await?
        .is_none()
    {
        return Ok(()); // branch appears with the first production render
    }
    match client
        .put(
            format!("/repos/{org}/ops/branches/env%2Fproduction/protection"),
            Some(&json!({
                "required_status_checks": null,
                "enforce_admins": false,
                "required_pull_request_reviews": { "required_approving_review_count": 1 },
                "restrictions": null,
                "allow_force_pushes": false,
                "allow_deletions": false,
            })),
        )
        .await
    {
        Ok::<serde_json::Value, _>(_) => {}
        // Branch protection needs a paid plan (private repos on Free 403).
        // Don't fail the whole sync — warn that the production gate isn't
        // enforced at the branch level (the render-PR review still applies).
        Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 403 => {
            tracing::warn!(
                org,
                "branch protection unavailable (paid GitHub plan required) — \
                 env/production is NOT branch-protected"
            );
        }
        Err(e) => return Err(e).context("protecting env/production"),
    }
    Ok(())
}

pub(crate) async fn protect_app_main(
    client: &octocrab::Octocrab,
    org: &str,
    app: &str,
) -> Result<()> {
    let _: serde_json::Value = client
        .put(
            format!("/repos/{org}/{app}/branches/main/protection"),
            Some(&json!({
                "required_status_checks": { "strict": false, "contexts": ["test"] },
                "enforce_admins": false,
                "required_pull_request_reviews": null,
                "restrictions": null,
                "allow_force_pushes": false,
                "allow_deletions": false,
            })),
        )
        .await
        .with_context(|| format!("protecting {app}@main"))?;
    Ok(())
}

/// Teams `admins` + `developers` per org, membership from project.yaml.
/// GitHub username is the identity everywhere (§9).
async fn sync_teams(client: &octocrab::Octocrab, org: &str, project: &ProjectConfig) -> Result<()> {
    for (team, role) in [("admins", Role::Admin), ("developers", Role::Developer)] {
        ensure_team(client, org, team).await?;
        let desired: Vec<&str> = project
            .members
            .iter()
            .filter(|m| m.role == role)
            .map(|m| m.user.as_str())
            .collect();

        let current: Vec<serde_json::Value> = client
            .get(format!("/orgs/{org}/teams/{team}/members"), None::<&()>)
            .await
            .unwrap_or_default();
        let current: Vec<String> = current
            .iter()
            .filter_map(|m| m["login"].as_str().map(String::from))
            .collect();

        for user in &desired {
            if !current.iter().any(|c| c.eq_ignore_ascii_case(user)) {
                let _: serde_json::Value = client
                    .put(
                        format!("/orgs/{org}/teams/{team}/memberships/{user}"),
                        Some(&json!({ "role": "member" })),
                    )
                    .await
                    .with_context(|| format!("adding {user} to {team}"))?;
                tracing::info!(org, team, user, "added team member");
            }
        }
        for user in &current {
            if !desired.iter().any(|d| d.eq_ignore_ascii_case(user)) {
                let route = format!("/orgs/{org}/teams/{team}/memberships/{user}");
                client
                    .delete::<serde_json::Value, _, ()>(route, None)
                    .await
                    .ok();
                tracing::info!(org, team, user, "removed team member");
            }
        }
    }
    Ok(())
}

async fn ensure_team(client: &octocrab::Octocrab, org: &str, team: &str) -> Result<()> {
    // Attempt creation and tolerate 422 ("Name/Slug must be unique") — the
    // team already exists. A read-then-create raced (and missed pre-existing
    // teams whose slug differs from the name), same class as the repo/ node
    // upsert fixes.
    match client
        .post(
            format!("/orgs/{org}/teams"),
            Some(&json!({ "name": team, "privacy": "closed" })),
        )
        .await
    {
        Ok::<serde_json::Value, _>(_) => Ok(()),
        Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 422 => Ok(()),
        Err(e) => Err(e).with_context(|| format!("creating team {team}")),
    }
}

/// name → archived, for all repos in the org.
async fn list_org_repos(client: &octocrab::Octocrab, org: &str) -> Result<BTreeMap<String, bool>> {
    let mut repos = BTreeMap::new();
    for page in 1..=10 {
        let batch: Vec<serde_json::Value> = client
            .get(
                format!("/orgs/{org}/repos?per_page=100&page={page}"),
                None::<&()>,
            )
            .await
            .context("listing org repos")?;
        if batch.is_empty() {
            break;
        }
        for repo in &batch {
            if let Some(name) = repo["name"].as_str() {
                repos.insert(
                    name.to_string(),
                    repo["archived"].as_bool().unwrap_or(false),
                );
            }
        }
    }
    Ok(repos)
}
