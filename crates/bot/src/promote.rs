//! Promotion (§8, §13): copy the digest currently running in `stable` into
//! the production overlay on ops `main`. The render pipeline then opens the
//! `env/production` render PR — and *that* admin review is the gate; this
//! endpoint only proposes.
//!
//! Reached via the WG-internal API (dashboard/CLI). Phase 5 adds acting-user
//! attribution via Tailscale identity headers (§16).

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use base64::Engine;
use std::sync::Arc;

use crate::AppState;

pub async fn promote(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
) -> Result<String, (StatusCode, String)> {
    do_promote(&state, &org, &app)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn do_promote(state: &AppState, org: &str, app: &str) -> Result<String> {
    anyhow::ensure!(
        app.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "invalid app name"
    );
    let client = state.github.org_client(org).await?;
    let repos = client.repos(org, "ops");

    let stable = read_file(&repos, &format!("apps/{app}/stable.yaml"))
        .await?
        .with_context(|| {
            format!("apps/{app}/stable.yaml not found — nothing deployed to stable")
        })?;
    let image = stable
        .0
        .lines()
        .find_map(|l| l.strip_prefix("image: "))
        .context("stable overlay has no image line")?
        .trim()
        .to_string();
    anyhow::ensure!(
        image.contains("@sha256:"),
        "stable image is not digest-pinned: {image}"
    );

    let production_path = format!("apps/{app}/production.yaml");
    let short = &image[image.len().saturating_sub(12)..];
    let message = format!("promote({app}): stable digest to production ({short})");
    match read_file(&repos, &production_path).await? {
        Some((current, sha)) => {
            let updated = crate::digest::replace_image_line(&current, &image)?;
            if updated == current {
                return Ok(format!("{app}: production already at {image}"));
            }
            repos
                .update_file(&production_path, &message, &updated, &sha)
                .branch("main")
                .send()
                .await?;
        }
        None => {
            let content = format!(
                "# production overlay for {app} — digest moves via promote\nimage: {image}\n"
            );
            repos
                .create_file(&production_path, &message, &content)
                .branch("main")
                .send()
                .await?;
        }
    }
    state
        .store
        .log_event("promote", Some(org), &format!("{app} → {image}"))?;
    tracing::info!(org, app, %image, "promoted — env/production render PR will follow");
    Ok(format!(
        "{app}: promotion committed; review the env/production render PR to deploy"
    ))
}

/// Rollback (§16): `git revert` of the latest change on ops `main` — a new
/// commit restoring the previous tree, so history stays append-only and the
/// render pipeline propagates the rollback like any other change.
pub async fn rollback(
    State(state): State<Arc<AppState>>,
    Path(org): Path<String>,
) -> Result<String, (StatusCode, String)> {
    do_rollback(&state, &org)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn do_rollback(state: &AppState, org: &str) -> Result<String> {
    let client = state.github.org_client(org).await?;
    let repo = format!("/repos/{org}/ops");

    let head = crate::git::get_branch_head(&client, &repo, "main")
        .await?
        .context("ops has no main branch")?;
    let commit: serde_json::Value = client
        .get(format!("{repo}/git/commits/{head}"), None::<&()>)
        .await?;
    let parents = commit["parents"].as_array().cloned().unwrap_or_default();
    anyhow::ensure!(
        parents.len() == 1,
        "head commit has {} parents — rollback needs a linear history step",
        parents.len()
    );
    let parent = parents[0]["sha"].as_str().context("parent has no sha")?;
    let parent_tree = crate::git::commit_tree(&client, &repo, parent).await?;

    let short = &head[..12.min(head.len())];
    let message = format!("revert: roll back {short}");
    let revert =
        crate::git::create_commit(&client, &repo, &parent_tree, &[&head], &message).await?;
    crate::git::force_update_ref(&client, &repo, "main", &revert).await?;

    state
        .store
        .log_event("rollback", Some(org), &format!("reverted {head}"))?;
    tracing::info!(org, head, "rolled back — render pipeline will propagate");
    Ok(format!("reverted {short}; render PRs will follow"))
}

/// (content, blob_sha) of a file on ops main, or None if absent.
async fn read_file(
    repos: &octocrab::repos::RepoHandler<'_>,
    path: &str,
) -> Result<Option<(String, String)>> {
    match repos.get_content().path(path).r#ref("main").send().await {
        Ok(content) => {
            let item = content
                .items
                .into_iter()
                .next()
                .context("empty contents response")?;
            let encoded = item
                .content
                .clone()
                .unwrap_or_default()
                .replace(['\n', ' '], "");
            let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
            Ok(Some((String::from_utf8(decoded)?, item.sha)))
        }
        Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 404 => Ok(None),
        Err(e) => Err(e).context("reading ops file"),
    }
}
