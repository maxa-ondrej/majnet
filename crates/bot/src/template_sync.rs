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
    Ok(synced)
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
