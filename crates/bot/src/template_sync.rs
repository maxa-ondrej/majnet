//! Template sync — propagate platform-managed template files into *existing* app
//! repos (the counterpart to `org_sync::create_repo_from_template`, which only
//! seeds NEW repos).
//!
//! Only the files in `MANAGED_FILES` are synced — the platform CI *contract*
//! (`release.yaml`, which just calls the reusable `app-release.yaml`). Everything
//! else — `build.yaml` (apps legitimately customize their build: pnpm, Docker-only,
//! etc.), the Dockerfile, and source scaffolds — is a one-time seed the developer
//! owns and may freely diverge, so it is never touched. When an app repo's managed
//! files drift from its declared template, this opens (or fast-forwards) a
//! `template-sync` PR on that repo — reviewable, never a force-push to `main`.
//! Idempotent: no drift → no PR. Extend `MANAGED_FILES` as more files become
//! genuinely platform-owned (stack-agnostic).
//!
//! Monorepos (ADR 0018) are bring-your-own CI, so they get no scaffolded
//! `build.yaml`. As a convenience, this also **seeds** a matrix build-tier caller
//! (`.github/workflows/build.yaml` → the reusable `app-build.yaml`, one matrix
//! entry per app) into a monorepo repo that lacks one — a one-time `monorepo-ci`
//! PR, never overwriting an existing file.

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use base64::Engine;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::dashboard_api::ApiError;
use crate::AppState;
use majnet_common::project::Role;

/// Repo-relative template files that are platform-managed (kept in sync). Only
/// the release contract for now — `build.yaml` and scaffolds are app-owned.
const MANAGED_FILES: &[&str] = &[".github/workflows/release.yaml"];
const SYNC_BRANCH: &str = "template-sync";

/// A monorepo (ADR 0018) is bring-your-own CI, so it ships no scaffolded
/// `build.yaml`. We seed a matrix caller for the reusable build-tier workflow
/// once (a convenience, never overwritten), on its own branch/PR.
const MONOREPO_CI_BRANCH: &str = "monorepo-ci";
const BUILD_CALLER_PATH: &str = ".github/workflows/build.yaml";

/// `POST /api/template-sync/{org}` — sync platform-managed template files into
/// the org's app repos, opening a `template-sync` PR per repo that has drifted.
/// Admin-gated (it opens PRs on source repos).
pub async fn sync_post(
    State(state): State<Arc<AppState>>,
    Path(org): Path<String>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    let actor = crate::authz::require(&state, &headers, &org, Role::Admin)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let synced = sync_org(&state, &org)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    state
        .store
        .log_event(
            "template-sync",
            Some(&org),
            &format!("by {actor}: {}", summary(&synced)),
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(if synced.is_empty() {
        "all app repos are up to date with their templates".to_string()
    } else {
        format!(
            "opened/updated template-sync PRs for: {}",
            synced.join(", ")
        )
    })
}

fn summary(synced: &[String]) -> String {
    if synced.is_empty() {
        "up to date".into()
    } else {
        format!("synced {}", synced.join(", "))
    }
}

/// Sync every app repo in the org against its declared template. Returns the
/// apps for which a `template-sync` PR was opened or updated.
pub async fn sync_org(state: &AppState, org: &str) -> Result<Vec<String>> {
    let (_, platform_tar) =
        crate::proxy::fetch_snapshot(state, &state.config.root_org, "platform", "main").await?;
    let platform = majnet_common::tarball::untar(&platform_tar)?;
    let project = crate::dashboard_api::read_project(state, org).await?;
    let client = state.github.org_client(org).await?;

    let mut synced = Vec::new();
    for app in &project.apps {
        // A monorepo member has no repo of its own named `<app>`; its CI is the
        // repo owner's (handled by the build-caller seeding below).
        if app.is_monorepo() {
            continue;
        }
        let managed = managed_files(&platform, &app.template, org, &app.name);
        if managed.is_empty() {
            continue; // template has no managed files (or is missing)
        }
        match sync_app(&client, org, &app.name, &managed).await {
            Ok(true) => synced.push(app.name.clone()),
            Ok(false) => {}
            Err(e) => tracing::error!(
                org,
                app = app.name,
                error = format!("{e:#}"),
                "template sync failed for app"
            ),
        }
    }

    // Seed a build-tier matrix caller into each BYO-CI monorepo that lacks one
    // (ADR 0018). One caller per shared repo, listing every app in it.
    let mut monorepo_repos: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for app in &project.apps {
        if app.is_monorepo() {
            monorepo_repos
                .entry(app.repo().to_string())
                .or_default()
                .push(app.name.clone());
        }
    }
    for (repo, mut apps) in monorepo_repos {
        apps.sort();
        match seed_monorepo_ci(&client, org, &repo, &apps).await {
            Ok(true) => synced.push(format!("{repo} (build CI)")),
            Ok(false) => {}
            Err(e) => tracing::error!(
                org,
                repo,
                error = format!("{e:#}"),
                "monorepo build-CI scaffold failed"
            ),
        }
    }
    Ok(synced)
}

/// A `build.yaml` matrix caller for a BYO-CI monorepo: one matrix entry per app
/// sharing the repo, each invoking the reusable `app-build.yaml`. `context`
/// defaults to the app name — the owner adjusts it to each app's build dir. A
/// one-time seed (never overwritten), so it's theirs to customize.
fn monorepo_build_caller(apps: &[String]) -> String {
    let matrix = apps
        .iter()
        .map(|a| format!("          - {{ name: {a}, context: {a} }}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"name: build

# Auto-scaffolded by MajNet for this monorepo (ADR 0018). Each app builds its
# own nested image ghcr.io/<org>/<repo>/<app> via the reusable build-tier
# workflow: pr-<N> -> preview, sha-/latest -> testing. Adjust each app's
# `context` to its build directory and add test steps as you like — MajNet
# seeds this file once and never overwrites it.
on:
  push:
    branches: [main]
  pull_request:

jobs:
  build:
    strategy:
      matrix:
        app:
{matrix}
    permissions: {{ contents: read, packages: write }}
    uses: majnet/majnet/.github/workflows/app-build.yaml@main
    with:
      app: ${{{{ matrix.app.name }}}}
      context: ${{{{ matrix.app.context }}}}
"#
    )
}

/// Seed the monorepo's build caller if it has none. Never overwrites an existing
/// `build.yaml` (the owner's). Opens (or fast-forwards) a `monorepo-ci` PR.
/// Returns whether a PR was opened/updated.
async fn seed_monorepo_ci(
    client: &octocrab::Octocrab,
    org: &str,
    repo: &str,
    apps: &[String],
) -> Result<bool> {
    let repo_path = format!("/repos/{org}/{repo}");
    let Some(main_head) = crate::git::get_branch_head(client, &repo_path, "main").await? else {
        return Ok(false); // repo absent / not initialized
    };
    // Seed only when absent — an existing build.yaml is the owner's to keep.
    let repos = client.repos(org, repo);
    if read_file(&repos, BUILD_CALLER_PATH).await.is_some() {
        return Ok(false);
    }

    let changes: BTreeMap<String, Option<String>> = BTreeMap::from([(
        BUILD_CALLER_PATH.to_string(),
        Some(monorepo_build_caller(apps)),
    )]);
    let base_tree = crate::git::commit_tree(client, &repo_path, &main_head).await?;
    let tree =
        crate::git::create_tree_incremental(client, &repo_path, &base_tree, &changes).await?;
    let commit = crate::git::create_commit(
        client,
        &repo_path,
        &tree,
        &[&main_head],
        "chore: scaffold MajNet build CI",
    )
    .await?;
    if crate::git::get_branch_head(client, &repo_path, MONOREPO_CI_BRANCH)
        .await?
        .is_some()
    {
        crate::git::force_update_ref(client, &repo_path, MONOREPO_CI_BRANCH, &commit).await?;
    } else {
        crate::git::create_ref(client, &repo_path, MONOREPO_CI_BRANCH, &commit).await?;
    }

    let open: serde_json::Value = client
        .get(
            format!("{repo_path}/pulls?state=open&base=main&head={org}:{MONOREPO_CI_BRANCH}"),
            None::<&()>,
        )
        .await?;
    if open.as_array().and_then(|prs| prs.first()).is_none() {
        let list = apps.join(", ");
        let _: serde_json::Value = client
            .post(
                format!("{repo_path}/pulls"),
                Some(&json!({
                    "title": "chore: scaffold MajNet build CI",
                    "head": MONOREPO_CI_BRANCH,
                    "base": "main",
                    "body": format!(
                        "MajNet scaffolds a build-tier workflow for this monorepo (ADR 0018): \
                         each app ({list}) builds its own nested image via the reusable \
                         `app-build.yaml` (pr-<N> → preview, sha-/latest → testing).\n\n\
                         **Adjust each app's `context`** to its build directory before merging. \
                         This file is yours after seeding — MajNet won't overwrite it."
                    ),
                })),
            )
            .await
            .context("opening monorepo-ci PR")?;
    }
    Ok(true)
}

/// The managed template files for an app (those in `MANAGED_FILES` present in the
/// template), keyed by repo-relative path, with `{{app}}`/`{{org}}` substituted
/// (matching `create_repo_from_template`).
fn managed_files(
    platform: &BTreeMap<String, Vec<u8>>,
    template: &str,
    org: &str,
    app: &str,
) -> BTreeMap<String, String> {
    let prefix = format!("repo-templates/{template}/");
    MANAGED_FILES
        .iter()
        .filter_map(|rel| {
            let content = platform.get(&format!("{prefix}{rel}"))?;
            let text = String::from_utf8(content.clone()).ok()?;
            Some((
                rel.to_string(),
                text.replace("{{app}}", app).replace("{{org}}", org),
            ))
        })
        .collect()
}

/// Ensure the app repo's managed files match the template; open/fast-forward a
/// `template-sync` PR if they drifted. Returns whether a PR was opened/updated.
async fn sync_app(
    client: &octocrab::Octocrab,
    org: &str,
    app: &str,
    managed: &BTreeMap<String, String>,
) -> Result<bool> {
    let repo = format!("/repos/{org}/{app}");
    let Some(main_head) = crate::git::get_branch_head(client, &repo, "main").await? else {
        return Ok(false); // repo not initialized yet
    };

    // Which managed files differ from (or are missing on) main?
    let repos = client.repos(org, app);
    let mut changes: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (rel, want) in managed {
        let have = read_file(&repos, rel).await;
        if have.as_deref() != Some(want.as_str()) {
            changes.insert(rel.clone(), Some(want.clone()));
        }
    }
    if changes.is_empty() {
        return Ok(false); // in sync
    }

    // Commit the managed changes on top of main into the template-sync branch.
    let base_tree = crate::git::commit_tree(client, &repo, &main_head).await?;
    let tree = crate::git::create_tree_incremental(client, &repo, &base_tree, &changes).await?;
    let commit = crate::git::create_commit(
        client,
        &repo,
        &tree,
        &[&main_head],
        "chore: sync repo template",
    )
    .await?;
    if crate::git::get_branch_head(client, &repo, SYNC_BRANCH)
        .await?
        .is_some()
    {
        crate::git::force_update_ref(client, &repo, SYNC_BRANCH, &commit).await?;
    } else {
        crate::git::create_ref(client, &repo, SYNC_BRANCH, &commit).await?;
    }

    // Ensure a single open PR (fast-forwarding the branch updates it in place).
    let open: serde_json::Value = client
        .get(
            format!("{repo}/pulls?state=open&base=main&head={org}:{SYNC_BRANCH}"),
            None::<&()>,
        )
        .await?;
    if open.as_array().and_then(|prs| prs.first()).is_none() {
        let files = changes.keys().cloned().collect::<Vec<_>>().join(", ");
        let _: serde_json::Value = client
            .post(
                format!("{repo}/pulls"),
                Some(&json!({
                    "title": "chore: sync repo template",
                    "head": SYNC_BRANCH,
                    "base": "main",
                    "body": format!(
                        "Platform-managed CI files drifted from the current template \
                         and are updated here.\n\nFiles: {files}\n\n\
                         Only the platform release contract is synced — your \
                         `build.yaml`, Dockerfile and source are never touched."
                    ),
                })),
            )
            .await
            .context("opening template-sync PR")?;
    }
    Ok(true)
}

/// Content of a file on the app repo's `main`, or None if absent/unreadable.
async fn read_file(repos: &octocrab::repos::RepoHandler<'_>, path: &str) -> Option<String> {
    let content = repos
        .get_content()
        .path(path)
        .r#ref("main")
        .send()
        .await
        .ok()?;
    let item = content.items.into_iter().next()?;
    let b64 = item.content?.replace(['\n', ' '], "");
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_files_picks_only_the_contract_and_substitutes() {
        let platform = BTreeMap::from([
            (
                "repo-templates/web-app/.github/workflows/release.yaml".to_string(),
                b"uses: majnet/majnet@main # {{org}}/{{app}}".to_vec(),
            ),
            // Not managed — apps customize their build.
            (
                "repo-templates/web-app/.github/workflows/build.yaml".to_string(),
                b"custom".to_vec(),
            ),
            (
                "repo-templates/web-app/Dockerfile".to_string(),
                b"FROM x".to_vec(),
            ),
        ]);
        let m = managed_files(&platform, "web-app", "myorg", "myapp");
        assert_eq!(m.len(), 1);
        assert_eq!(
            m[".github/workflows/release.yaml"],
            "uses: majnet/majnet@main # myorg/myapp"
        );
    }

    #[test]
    fn missing_template_yields_nothing() {
        let platform = BTreeMap::new();
        assert!(managed_files(&platform, "web-app", "o", "a").is_empty());
    }

    #[test]
    fn monorepo_build_caller_is_valid_yaml_with_an_entry_per_app() {
        let out = monorepo_build_caller(&["api".to_string(), "web".to_string()]);
        // Parses as a single YAML document.
        let v: serde_yaml::Value = serde_yaml::from_str(&out).expect("valid workflow YAML");
        // One matrix entry per app, each with a name + context.
        let apps = v["jobs"]["build"]["strategy"]["matrix"]["app"]
            .as_sequence()
            .expect("matrix.app is a sequence");
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0]["name"], serde_yaml::Value::from("api"));
        assert_eq!(apps[0]["context"], serde_yaml::Value::from("api"));
        assert_eq!(apps[1]["name"], serde_yaml::Value::from("web"));
        // Calls the reusable build-tier workflow, forwarding the matrix entry.
        assert_eq!(
            v["jobs"]["build"]["uses"],
            serde_yaml::Value::from("majnet/majnet/.github/workflows/app-build.yaml@main")
        );
        assert_eq!(
            v["jobs"]["build"]["with"]["app"],
            serde_yaml::Value::from("${{ matrix.app.name }}")
        );
    }
}
