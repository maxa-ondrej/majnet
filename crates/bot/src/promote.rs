//! Promotion (§8, §13): copy the digest currently running in `stable` into
//! the production overlay on ops `main`. The render pipeline then opens the
//! `env/production` render PR — and *that* admin review is the gate; this
//! endpoint only proposes.
//!
//! Reached via the WG-internal API (dashboard/CLI). Phase 5 adds acting-user
//! attribution via Tailscale identity headers (§16).

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use base64::Engine;
use majnet_common::project::Role;
use std::sync::Arc;

use crate::AppState;

pub async fn promote(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<String, (StatusCode, String)> {
    // Promotion is a production action (§9) — project admins only.
    let actor = crate::authz::require(&state, &headers, &org, Role::Admin)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    do_promote(&state, &org, &app, &actor)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn do_promote(state: &AppState, org: &str, app: &str, actor: &str) -> Result<String> {
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
    // Promotion pins ONLY the digest: read whatever `stable` is running (a
    // `digest:` field, or the pin on a legacy combined `image:`) and copy that
    // digest into `production` — the bare repository is inherited from base.yaml.
    let digest = crate::digest::overlay_digest(&stable.0)
        .context("stable overlay carries no digest pin — nothing deployed to stable")?;

    let production_path = format!("apps/{app}/production.yaml");
    let hex = digest.strip_prefix("sha256:").unwrap_or(&digest);
    let short = &hex[..12.min(hex.len())];
    let message = format!("promote({app}): stable digest to production ({short})");
    match read_file(&repos, &production_path).await? {
        Some((current, sha)) => {
            let updated = crate::digest::replace_digest_line(&current, &digest)?;
            if updated == current {
                return Ok(format!("{app}: production already at {digest}"));
            }
            repos
                .update_file(&production_path, &message, &updated, &sha)
                .branch("main")
                .send()
                .await?;
        }
        None => {
            let content = format!(
                "# production overlay for {app} — digest moves via promote\ndigest: {digest}\n"
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
        .log_event("promote", Some(org), &format!("{app} → {digest} by {actor}"))?;
    tracing::info!(org, app, %digest, actor, "promoted — env/production render PR will follow");
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
    headers: HeaderMap,
) -> Result<String, (StatusCode, String)> {
    // Reverting ops main can touch production overlays — admins only.
    let actor = crate::authz::require(&state, &headers, &org, Role::Admin)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    do_rollback(&state, &org, &actor)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn do_rollback(state: &AppState, org: &str, actor: &str) -> Result<String> {
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

    state.store.log_event(
        "rollback",
        Some(org),
        &format!("reverted {head} by {actor}"),
    )?;
    tracing::info!(
        org,
        head,
        actor,
        "rolled back — render pipeline will propagate"
    );
    Ok(format!("reverted {short}; render PRs will follow"))
}

/// (content, blob_sha) of a file on ops main, or None if absent.
pub(crate) async fn read_file(
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
