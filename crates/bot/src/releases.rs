//! Releases (ADR 0009): the bot watches app repos' GitHub Releases, reads the
//! `majnet-release.yaml` descriptor at the tag, records it, and promotes a
//! chosen release into `ops` production (stable auto-tracks the latest tag).
//!
//! The `release` webhook is the fast path; `backfill_app` is a periodic
//! reconcile (from the hourly org-sync) that recovers missed or out-of-order
//! deliveries by listing releases from GitHub and healing any stable drift.

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use majnet_common::manifest::Migration;
use majnet_common::project::Role;
use majnet_common::release::Release;
use secrecy::ExposeSecret;
use std::sync::Arc;

use axum::Json;

use crate::state::StoredRelease;
use crate::AppState;

type ApiError = (StatusCode, String);

const DESCRIPTOR: &str = "majnet-release.yaml";

/// Handle a `release` webhook: on publish, read + validate the `majnet-release.yaml`
/// **release asset** (CI computes the digests at build time, after the tag, so the
/// descriptor is an asset, not a committed file) and record it. A release without
/// that asset isn't a MajNet release — we log and move on rather than error.
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

    let Some(asset_url) = descriptor_asset_url(release) else {
        tracing::info!(org, app, tag, "release has no {DESCRIPTOR} asset — skipping");
        return Ok(());
    };
    let descriptor = fetch_descriptor(state, org, &asset_url)
        .await
        .with_context(|| format!("{org}/{app}@{tag}: {DESCRIPTOR}"))?;

    state
        .store
        .upsert_release(org, app, &descriptor, published_at)?;
    state.store.log_event(
        "release-published",
        Some(org),
        &format!("{app} {} ({})", descriptor.version, &descriptor.app),
    )?;
    tracing::info!(org, app, version = %descriptor.version, "release recorded");

    track_stable(state, org, app).await
}

/// Periodic release backfill (ADR 0009 open item): the `release` webhook is the
/// fast path, but a missed or out-of-order delivery (e.g. the descriptor asset
/// attached just after publish) would leave the store — and `stable` — stale.
/// This lists an app's GitHub releases, records any carrying a
/// `majnet-release.yaml` asset we haven't seen, and re-points stable at the
/// latest. Idempotent: already-recorded versions are skipped, and `track_stable`
/// heals stable drift even when nothing new was found. Called from the hourly
/// org-sync reconcile.
pub async fn backfill_app(state: &AppState, org: &str, app: &str) -> Result<()> {
    let client = state.github.org_client(org).await?;
    let known: std::collections::HashSet<String> = state
        .store
        .releases(org, app)?
        .into_iter()
        .map(|r| r.version)
        .collect();

    let mut recorded = 0u32;
    // Recent releases first; a few pages cover any realistic missed backlog.
    for page in 1..=3 {
        let batch: Vec<serde_json::Value> = client
            .get(
                format!("/repos/{org}/{app}/releases?per_page=100&page={page}"),
                None::<&()>,
            )
            .await
            .with_context(|| format!("listing releases for {org}/{app}"))?;
        if batch.is_empty() {
            break;
        }
        for release in &batch {
            let tag = release["tag_name"].as_str().unwrap_or_default();
            if tag.is_empty() || known.contains(tag) {
                continue;
            }
            let published_at = release["published_at"].as_str().unwrap_or_default();
            let Some(asset_url) = descriptor_asset_url(release) else {
                continue; // not a MajNet release
            };
            match fetch_descriptor(state, org, &asset_url).await {
                Ok(descriptor) => {
                    state.store.upsert_release(org, app, &descriptor, published_at)?;
                    recorded += 1;
                    tracing::info!(org, app, tag, "backfilled release");
                }
                Err(e) => {
                    tracing::warn!(org, app, tag, error = %format!("{e:#}"), "backfill: bad descriptor")
                }
            }
        }
    }

    if recorded > 0 {
        state.store.log_event(
            "release-backfill",
            Some(org),
            &format!("{app}: {recorded} recorded"),
        )?;
    }
    // Reconcile stable even when nothing new was recorded — a webhook that
    // recorded a release but failed to bump stable is healed here.
    track_stable(state, org, app).await
}

/// The API URL of a release's `majnet-release.yaml` asset, if it carries one.
fn descriptor_asset_url(release: &serde_json::Value) -> Option<String> {
    release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str() == Some(DESCRIPTOR))
        .and_then(|a| a["url"].as_str())
        .map(String::from)
}

/// Download + validate a release descriptor from its asset API URL. The URL
/// serves the raw bytes when requested with `Accept: application/octet-stream`.
async fn fetch_descriptor(state: &AppState, org: &str, asset_url: &str) -> Result<Release> {
    let (_, token) = state.github.org_client_and_token(org).await?;
    let bytes = state
        .http
        .get(asset_url)
        .bearer_auth(token.expose_secret())
        .header(header::ACCEPT, "application/octet-stream")
        .header(header::USER_AGENT, "majnet-bot")
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("downloading {DESCRIPTOR} from {asset_url}"))?
        .bytes()
        .await?;
    Release::parse(&bytes).with_context(|| format!("invalid {DESCRIPTOR}"))
}

/// Re-point `apps/{app}/stable.yaml` at the newest recorded release (ADR 0009
/// phase 5). Opt-in via overlay-presence; a no-op when stable is already
/// current or the app has no releases. The store orders by publish time, so an
/// edit to an older release never demotes stable off the true latest;
/// `production` moves only via promote.
async fn track_stable(state: &AppState, org: &str, app: &str) -> Result<()> {
    let Some(latest) = state.store.releases(org, app)?.into_iter().next() else {
        return Ok(());
    };
    if crate::digest::bump_class_digest(state, org, app, &latest.app_image, "stable").await? {
        state.store.log_event(
            "digest-bump",
            Some(org),
            &format!("{app} stable → {} ({})", latest.version, latest.app_image),
        )?;
    }
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

#[derive(serde::Serialize)]
struct ProdOverlay {
    image: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    migration: Option<Migration>,
}

/// `POST /api/releases/{org}/{app}/promote/{version}` — pin production to a
/// chosen release (ADR 0009): write its app + migration digests into
/// `apps/{app}/production.yaml` on ops main. Admin-gated; the `env/production`
/// render PR (the §9 gate) follows. Stable auto-tracks the latest tag, so
/// promotion targets production only.
pub async fn promote(
    State(state): State<Arc<AppState>>,
    Path((org, app, version)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    let actor = crate::authz::require(&state, &headers, &org, Role::Admin)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;

    let rel = state
        .store
        .releases(&org, &app)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter()
        .find(|r| r.version == version)
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("release {version} not found for {app}"),
        ))?;

    // Overlay pins the app image; migration (if any) carries its own image, or
    // omits it to run in the app image (Migration defaults on the reconciler).
    let migration = rel.migration_command.clone().map(|command| Migration {
        image: rel.migration_image.clone(),
        command,
    });
    let overlay = format!(
        "# production overlay for {app} — release {version} (ADR 0009)\n{}",
        serde_yaml::to_string(&ProdOverlay {
            image: rel.app_image.clone(),
            migration,
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    );

    // Validate base ⊕ this overlay before committing.
    let mut files = crate::dashboard_api::app_files(&state, &org, &app)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    files.insert("production.yaml".to_string(), overlay.clone());
    crate::dashboard_api::validate_app_files(&app, &files)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;

    crate::dashboard_api::commit_file(
        &state,
        &org,
        &format!("apps/{app}/production.yaml"),
        &overlay,
        &format!("promote({app}): release {version} to production by {actor}"),
    )
    .await
    .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;

    state
        .store
        .log_event(
            "promote-release",
            Some(&org),
            &format!("{app} {version} by {actor}"),
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(format!(
        "{app} {version} promoted; review the env/production render PR to deploy"
    ))
}

#[cfg(test)]
mod tests {
    use super::descriptor_asset_url;
    use serde_json::json;

    #[test]
    fn finds_the_descriptor_asset_url() {
        let release = json!({
            "assets": [
                { "name": "other.txt", "url": "https://api/assets/1" },
                { "name": "majnet-release.yaml", "url": "https://api/assets/2" },
            ]
        });
        assert_eq!(
            descriptor_asset_url(&release).as_deref(),
            Some("https://api/assets/2")
        );
    }

    #[test]
    fn no_descriptor_asset_is_none() {
        let release = json!({ "assets": [{ "name": "sbom.json", "url": "u" }] });
        assert!(descriptor_asset_url(&release).is_none());
        // A release with no assets array at all is also handled.
        assert!(descriptor_asset_url(&json!({})).is_none());
    }
}
