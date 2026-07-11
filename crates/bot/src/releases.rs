//! Releases (ADR 0009): the bot watches app repos' GitHub Releases, reads the
//! `majnet-release.yaml` descriptor at the tag, and records it. This is the
//! DEV-side of delivery; promotion into `ops` (stable/production) is phase 3.

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use base64::Engine;
use majnet_common::release::Release;
use std::sync::Arc;

use crate::state::StoredRelease;
use crate::AppState;

const DESCRIPTOR: &str = "majnet-release.yaml";

/// Handle a `release` webhook: on publish, read + validate the descriptor at the
/// tag and record it. A repo without a `majnet-release.yaml` isn't a MajNet
/// release — we log and move on rather than error.
pub async fn on_release(state: &AppState, org: &str, payload: &serde_json::Value) -> Result<()> {
    let action = payload["action"].as_str().unwrap_or_default();
    if !matches!(action, "published" | "released" | "edited") {
        return Ok(());
    }
    let app = payload["repository"]["name"].as_str().unwrap_or_default();
    if app.is_empty() || app == "ops" {
        return Ok(());
    }
    let release = &payload["release"];
    let tag = release["tag_name"].as_str().unwrap_or_default();
    let published_at = release["published_at"].as_str().unwrap_or_default();
    if tag.is_empty() {
        return Ok(());
    }

    let client = state.github.org_client(org).await?;
    let Some(bytes) = read_at_ref(&client, org, app, DESCRIPTOR, tag).await? else {
        tracing::info!(
            org,
            app,
            tag,
            "release has no {DESCRIPTOR} — not a MajNet release"
        );
        return Ok(());
    };
    let descriptor = Release::parse(&bytes)
        .with_context(|| format!("{org}/{app}@{tag}: invalid {DESCRIPTOR}"))?;

    state
        .store
        .upsert_release(org, app, &descriptor, published_at)?;
    state.store.log_event(
        "release-published",
        Some(org),
        &format!("{app} {} ({})", descriptor.version, &descriptor.app),
    )?;
    tracing::info!(org, app, version = %descriptor.version, "release recorded");
    Ok(())
}

/// `GET /api/releases/{org}/{app}` — recorded releases, newest first.
pub async fn list(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
) -> Result<Json<Vec<StoredRelease>>, (StatusCode, String)> {
    state
        .store
        .releases(&org, &app)
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Read a repo file at a specific ref (tag/branch/sha) via the Contents API.
/// `None` for a missing file (404).
async fn read_at_ref(
    client: &octocrab::Octocrab,
    org: &str,
    repo: &str,
    path: &str,
    r#ref: &str,
) -> Result<Option<Vec<u8>>> {
    match client
        .repos(org, repo)
        .get_content()
        .path(path)
        .r#ref(r#ref)
        .send()
        .await
    {
        Ok(content) => {
            let item = content
                .items
                .into_iter()
                .next()
                .context("empty contents response")?;
            let encoded = item.content.unwrap_or_default().replace(['\n', ' '], "");
            Ok(Some(
                base64::engine::general_purpose::STANDARD.decode(encoded)?,
            ))
        }
        Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 404 => Ok(None),
        Err(e) => Err(e).context("reading release descriptor"),
    }
}
