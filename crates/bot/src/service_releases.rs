//! Service releases + promote (ADR 0021 follow-up).
//!
//! A *service* (ADR 0021) runs an off-the-shelf image pinned by digest with no
//! source repo or CI, so it has none of the app release machinery. This surfaces
//! the **available upstream versions** (the image repo's registry tags) and a
//! one-click **promote**: resolve a chosen version → its digest → rewrite the
//! service manifest's `image:` on ops `main`. The render pipeline then opens the
//! gated `env/production` PR, exactly like an app promotion.
//!
//! Version *detection* (what's running now) lives in the reconciler, which reads
//! the image's `org.opencontainers.image.version` OCI label at deploy time — the
//! dashboard already shows it via the build-info card. This module is the
//! release/promote side and only talks to the public registry (anonymous pull
//! token), never the private-image PAT.

use anyhow::{bail, Context, Result};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use majnet_common::project::Role;
use std::sync::Arc;

use crate::AppState;

type ApiError = (StatusCode, String);
const GHCR: &str = "https://ghcr.io";
const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json";

#[derive(serde::Serialize)]
pub struct ServiceReleases {
    /// The image repo, e.g. `ghcr.io/pelican-dev/panel`.
    pub image_repo: String,
    /// The currently pinned image ref (`…@sha256:…`).
    pub current_image: String,
    /// Available upstream versions, newest first (version-like tags only).
    pub versions: Vec<String>,
}

/// `GET /api/service-releases/{org}/{app}` — available upstream versions for a
/// service's image. Empty `versions` (with the repo) when the registry isn't
/// GHCR or exposes no version-like tags — the dashboard still shows the current
/// version. Read-only; not gated.
pub async fn list(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
) -> Result<Json<ServiceReleases>, ApiError> {
    let image = service_image(&state, &org, &app)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    let Some(repo) = ghcr_repo(&image) else {
        // Not a GHCR image — nothing to enumerate, but report the current pin.
        return Ok(Json(ServiceReleases {
            image_repo: image.split(['@', ':']).next().unwrap_or(&image).to_string(),
            current_image: image,
            versions: vec![],
        }));
    };
    let versions = match available_versions(&state.http, &repo).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(org, app, %repo, error = format!("{e:#}"), "listing service versions failed");
            vec![]
        }
    };
    Ok(Json(ServiceReleases {
        image_repo: format!("ghcr.io/{repo}"),
        current_image: image,
        versions,
    }))
}

#[derive(serde::Deserialize)]
pub struct PromoteQuery {
    pub version: String,
}

/// `POST /api/service-releases/{org}/{app}/promote?version=…` — pin the chosen
/// version's digest into the service manifest on ops `main`. The render pipeline
/// opens the gated `env/production` PR; merging it deploys. Admin-gated.
pub async fn promote(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
    Query(q): Query<PromoteQuery>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    let actor = crate::authz::require(&state, &headers, &org, Role::Admin)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    do_promote(&state, &org, &app, &q.version, &actor)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn do_promote(
    state: &AppState,
    org: &str,
    app: &str,
    version: &str,
    actor: &str,
) -> Result<String> {
    let base = read_base(state, org, app).await?;
    let current = image_line(&base).context("service base.yaml has no image line")?;
    let repo = ghcr_repo(&current).context("service image is not a ghcr.io image")?;

    let token = public_token(&state.http, &repo).await?;
    let digest = resolve_digest(&state.http, &repo, version, &token).await?;
    let new_image = format!("ghcr.io/{repo}@{digest}");
    if current == new_image {
        return Ok(format!("{app}: already on {version} ({digest})"));
    }

    let updated = replace_image_line(&base, &new_image);
    crate::dashboard_api::commit_file(
        state,
        org,
        &format!("apps/{app}/base.yaml"),
        &updated,
        &format!("promote({app}): {version} ({})", short(&digest)),
    )
    .await?;
    state.store.log_event(
        "service-promote",
        Some(org),
        &format!("{app} → {version} ({new_image}) by {actor}"),
    )?;
    tracing::info!(org, app, version, %new_image, actor, "service promoted — render PR follows");
    Ok(format!(
        "{app}: promoted to {version}; review the env/production render PR to deploy"
    ))
}

// ── ops-repo manifest helpers ────────────────────────────────────────────────

async fn read_base(state: &AppState, org: &str, app: &str) -> Result<String> {
    let (_, tar) = crate::proxy::fetch_snapshot(state, org, "ops", "main").await?;
    let files = majnet_common::tarball::untar(&tar)?;
    let base = files
        .get(&format!("apps/{app}/base.yaml"))
        .with_context(|| format!("{org}/ops has no apps/{app}/base.yaml"))?;
    Ok(String::from_utf8_lossy(base).into_owned())
}

async fn service_image(state: &AppState, org: &str, app: &str) -> Result<String> {
    let base = read_base(state, org, app).await?;
    image_line(&base).with_context(|| format!("{app}/base.yaml has no image line"))
}

/// The value of the top-level `image:` key (`ghcr.io/…@sha256:…`).
fn image_line(yaml: &str) -> Option<String> {
    yaml.lines().find_map(|l| {
        let t = l.trim_start();
        // top-level key only (no indentation) — avoids matching nested `image:`.
        (l == t)
            .then(|| t.strip_prefix("image:"))
            .flatten()
            .map(|v| v.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

/// Rewrite the top-level `image:` line, preserving everything else verbatim.
fn replace_image_line(yaml: &str, new_image: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in yaml.lines() {
        if !replaced && line == line.trim_start() && line.trim_start().starts_with("image:") {
            out.push(format!("image: {new_image}"));
            replaced = true;
        } else {
            out.push(line.to_string());
        }
    }
    let mut s = out.join("\n");
    if yaml.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// `ghcr.io/<repo>[@digest|:tag]` → `<repo>` (e.g. `pelican-dev/panel`).
fn ghcr_repo(image: &str) -> Option<String> {
    let rest = image.strip_prefix("ghcr.io/")?;
    let repo = rest.split(['@', ':']).next()?;
    (!repo.is_empty()).then(|| repo.to_string())
}

fn short(digest: &str) -> &str {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    &hex[..hex.len().min(12)]
}

// ── public GHCR registry (anonymous) ─────────────────────────────────────────

async fn available_versions(http: &reqwest::Client, repo: &str) -> Result<Vec<String>> {
    let token = public_token(http, repo).await?;
    let mut tags: Vec<String> = list_tags(http, repo, &token)
        .await?
        .into_iter()
        .filter(|t| is_version(t))
        .collect();
    // Newest first, numeric-aware (so `beta34` sorts above `beta9`).
    tags.sort_by_key(|t| std::cmp::Reverse(version_key(t)));
    tags.dedup();
    Ok(tags)
}

async fn public_token(http: &reqwest::Client, repo: &str) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct T {
        #[serde(alias = "access_token")]
        token: String,
    }
    let url = format!("{GHCR}/token?service=ghcr.io&scope=repository:{repo}:pull");
    let r = http.get(&url).send().await?.error_for_status()?;
    Ok(r.json::<T>().await?.token)
}

async fn list_tags(http: &reqwest::Client, repo: &str, token: &str) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Tags {
        tags: Vec<String>,
    }
    let url = format!("{GHCR}/v2/{repo}/tags/list?n=1000");
    let r = http
        .get(&url)
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?;
    Ok(r.json::<Tags>().await?.tags)
}

async fn resolve_digest(
    http: &reqwest::Client,
    repo: &str,
    tag: &str,
    token: &str,
) -> Result<String> {
    let r = http
        .get(format!("{GHCR}/v2/{repo}/manifests/{tag}"))
        .bearer_auth(token)
        .header("Accept", ACCEPT)
        .send()
        .await?;
    let status = r.status();
    let digest = r
        .headers()
        .get("docker-content-digest")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if !status.is_success() {
        bail!("resolving {repo}:{tag} ({status})");
    }
    digest.context("registry response missing Docker-Content-Digest")
}

/// Version-like tag: starts with a digit or `v<digit>` (skips `latest`, `main`,
/// `sha-…`, branch tags).
fn is_version(tag: &str) -> bool {
    let t = tag.strip_prefix('v').unwrap_or(tag);
    t.chars().next().is_some_and(|c| c.is_ascii_digit()) && !tag.starts_with("sha-")
}

/// Numeric-aware sort key: the sequence of digit-runs in the tag, so
/// `v1.0.0-beta34` (`[1,0,0,34]`) orders above `v1.0.0-beta9` (`[1,0,0,9]`).
fn version_key(tag: &str) -> Vec<u64> {
    let mut nums = Vec::new();
    let mut cur = String::new();
    for c in tag.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
        } else if !cur.is_empty() {
            nums.push(cur.parse().unwrap_or(0));
            cur.clear();
        }
    }
    if !cur.is_empty() {
        nums.push(cur.parse().unwrap_or(0));
    }
    nums
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_rewrites_image_line() {
        let y =
            "name: pelican\nimage: ghcr.io/pelican-dev/panel@sha256:aaa\nenv:\n  image: nested\n";
        assert_eq!(
            image_line(y).as_deref(),
            Some("ghcr.io/pelican-dev/panel@sha256:aaa")
        );
        let out = replace_image_line(y, "ghcr.io/pelican-dev/panel@sha256:bbb");
        assert!(out.contains("image: ghcr.io/pelican-dev/panel@sha256:bbb"));
        assert!(out.contains("  image: nested"), "nested image untouched");
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn ghcr_repo_extraction() {
        assert_eq!(
            ghcr_repo("ghcr.io/pelican-dev/panel@sha256:x").as_deref(),
            Some("pelican-dev/panel")
        );
        assert_eq!(
            ghcr_repo("ghcr.io/pelican-dev/panel:v1").as_deref(),
            Some("pelican-dev/panel")
        );
        assert_eq!(ghcr_repo("docker.io/library/nginx@sha256:x"), None);
    }

    #[test]
    fn version_filtering_and_ordering() {
        let mut tags = vec![
            "latest".to_string(),
            "v1.0.0-beta9".to_string(),
            "v1.0.0-beta34".to_string(),
            "v1.0.0-beta11".to_string(),
            "sha-abc".to_string(),
            "main".to_string(),
        ];
        tags.retain(|t| is_version(t));
        tags.sort_by_key(|t| std::cmp::Reverse(version_key(t)));
        assert_eq!(tags, vec!["v1.0.0-beta34", "v1.0.0-beta11", "v1.0.0-beta9"]);
    }
}
