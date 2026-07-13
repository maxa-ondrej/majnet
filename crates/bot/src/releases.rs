//! Releases (ADR 0009): a **release is a `vX.Y.Z`-tagged image publish**. The
//! app's CI builds + pushes `ghcr.io/<org>/<app>:vX.Y.Z` by digest; the
//! `registry_package` webhook (which already drives the testing/ephemeral
//! digest bumps) carries the tag + digest, and the bot records it here. There
//! is no separate release descriptor — the digest comes off the webhook and the
//! migration lives in the ops overlay (`base.yaml`), next to the DB/secret
//! config it depends on. `stable` auto-tracks the latest tag; `promote` pins a
//! chosen version into `production.yaml`.

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use majnet_common::project::Role;
use std::sync::Arc;

use crate::state::StoredRelease;
use crate::AppState;

type ApiError = (StatusCode, String);

/// Record a `vX.Y.Z` release seen on a `registry_package` publish: resolve the
/// tag's commit (best-effort provenance), store it, and re-point stable at the
/// newest tag. `app_image` is the digest-pinned `ghcr.io/<org>/<app>@sha256:…`.
pub async fn record(
    state: &AppState,
    org: &str,
    app: &str,
    version: &str,
    app_image: &str,
) -> Result<()> {
    let commit = resolve_commit(state, org, app, version)
        .await
        .unwrap_or_default();
    state
        .store
        .upsert_release(org, app, version, &commit, app_image)?;
    state.store.log_event(
        "release-published",
        Some(org),
        &format!("{app} {version} ({app_image})"),
    )?;
    tracing::info!(org, app, version, "release recorded");
    track_stable(state, org, app).await
}

/// Resolve a tag to its commit SHA via the commits API, which follows both
/// lightweight and annotated tags. Best-effort — provenance, not correctness.
async fn resolve_commit(state: &AppState, org: &str, app: &str, tag: &str) -> Result<String> {
    let client = state.github.org_client(org).await?;
    let commit: serde_json::Value = client
        .get(format!("/repos/{org}/{app}/commits/{tag}"), None::<&()>)
        .await?;
    commit["sha"]
        .as_str()
        .map(String::from)
        .context("commit lookup returned no sha")
}

/// Re-point `apps/{app}/stable.yaml` at the newest recorded release (ADR 0009
/// phase 5). Opt-in via overlay-presence; a no-op when stable is already
/// current or the app has no releases. The store orders by publish time, so
/// stable stays on the true latest; `production` moves only via promote.
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

/// Backfill releases for `org/app` from GHCR package versions (ADR 0009 open
/// item). The `registry_package` webhook is the fast path, but a missed
/// delivery leaves the store (and stable) unaware of a `vX.Y.Z` publish with no
/// self-heal. The tag→digest map on the registry is authoritative, so this
/// enumerates every container version, and records each version-tagged one that
/// isn't already known (idempotent — `record` upserts + re-tracks stable).
/// Returns how many *new* releases were recorded. Needs `packages:read` on the
/// installation token.
pub async fn backfill(state: &AppState, org: &str, app: &str) -> Result<usize> {
    let mut known: std::collections::HashSet<String> = state
        .store
        .releases(org, app)?
        .into_iter()
        .map(|r| r.version)
        .collect();
    let client = state.github.org_client(org).await?;
    let mut recorded = 0;
    // Paginate defensively (cap at 10×100 versions) so a huge package can't spin
    // forever; a break on a short page ends it early.
    for page in 1..=10u32 {
        let versions: Vec<serde_json::Value> = client
            .get(
                format!(
                    "/orgs/{org}/packages/container/{app}/versions?per_page=100&page={page}"
                ),
                None::<&()>,
            )
            .await
            .context("listing GHCR package versions (needs packages:read)")?;
        let count = versions.len();
        for v in &versions {
            let Some(digest) = v["name"].as_str().filter(|d| d.starts_with("sha256:")) else {
                continue;
            };
            let Some(tags) = v["metadata"]["container"]["tags"].as_array() else {
                continue;
            };
            for tag in tags.iter().filter_map(|t| t.as_str()) {
                if crate::digest::is_version_tag(tag) && known.insert(tag.to_string()) {
                    let image = format!("ghcr.io/{org}/{app}@{digest}");
                    record(state, org, app, tag, &image).await?;
                    recorded += 1;
                }
            }
        }
        if count < 100 {
            break;
        }
    }
    tracing::info!(org, app, recorded, "release backfill complete");
    Ok(recorded)
}

/// `POST /api/releases/{org}/{app}/backfill` — recover missed releases from the
/// registry (ADR 0009 open item). Developer-gated (a stable-class recovery, not
/// a production change — production still moves only via promote).
pub async fn backfill_post(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    crate::authz::require(&state, &headers, &org, Role::Developer)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let n = backfill(&state, &org, &app)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(format!("backfilled {n} release(s) for {app} from the registry"))
}

/// `GET /api/releases/{org}/{app}` — recorded releases, newest first.
pub async fn list(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
) -> Result<Json<Vec<StoredRelease>>, ApiError> {
    state
        .store
        .releases(&org, &app)
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Build the `production.yaml` for a promote. Promote pins only the digest, so
/// replace the top-level `image:` in the existing overlay — preserving custom
/// ingress domains, env, and anything else hand-managed there (ADR 0013). When
/// the app has no production overlay yet, create a minimal image-only one.
fn production_overlay(
    current: Option<&str>,
    app: &str,
    version: &str,
    image: &str,
) -> Result<String> {
    match current {
        Some(existing) if !existing.trim().is_empty() => {
            crate::digest::replace_image_line(existing, image)
        }
        _ => Ok(format!(
            "# production overlay for {app} — release {version} (ADR 0009)\nimage: {image}\n"
        )),
    }
}

/// `POST /api/releases/{org}/{app}/promote/{version}` — pin production to a
/// chosen release (ADR 0009): update the app digest in
/// `apps/{app}/production.yaml` on ops main, leaving the rest of the overlay
/// (custom domains, env) untouched. The migration is inherited from `base.yaml`
/// (version-independent command; the files travel in the image), so the overlay
/// pins only the image. Admin-gated; the `env/production` render PR (the §9
/// gate) follows. Stable auto-tracks the latest tag, so promotion targets
/// production only.
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

    // Validate base ⊕ this overlay before committing.
    let mut files = crate::dashboard_api::app_files(&state, &org, &app)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;

    // Promote pins ONLY the image: replace the digest in the existing
    // production overlay, preserving any hand-managed production config
    // (custom ingress domains, env) rather than overwriting it — production
    // ingress lives in `production.yaml` by design (ADR 0013).
    let overlay = production_overlay(
        files.get("production.yaml").map(String::as_str),
        &app,
        &version,
        &rel.app_image,
    )
    .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
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
    use super::production_overlay;

    const NEW: &str = "ghcr.io/o/a@sha256:new";

    #[test]
    fn promote_preserves_hand_managed_production_config() {
        // The drift case: ingress was hand-added to production.yaml. A promote
        // must swap only the image and keep the ingress (ADR 0013).
        let current = "image: ghcr.io/o/a@sha256:old\ningress:\n  host: a.example.com\n  port: 8080\n";
        let out = production_overlay(Some(current), "a", "v1.2.3", NEW).unwrap();
        assert!(out.contains("image: ghcr.io/o/a@sha256:new"));
        assert!(out.contains("host: a.example.com"));
        assert!(out.contains("port: 8080"));
        assert!(!out.contains("sha256:old"));
    }

    #[test]
    fn promote_creates_minimal_overlay_when_absent() {
        for current in [None, Some(""), Some("   \n")] {
            let out = production_overlay(current, "a", "v1.0.0", NEW).unwrap();
            assert!(out.contains("image: ghcr.io/o/a@sha256:new"));
            assert!(out.contains("release v1.0.0"));
        }
    }
}
