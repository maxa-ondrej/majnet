//! Control-plane self-update (ADR 0005/0008) — the platform-admin surface behind
//! the dashboard's Control plane page. It reads the running pin from the
//! platform repo's `version.yaml`, resolves the latest CI build of the
//! control-plane + dashboard images, and — through git, like every other write —
//! commits a new pin. `master-1`'s `majnet-update` timer notices the change and
//! runs the blue-green rollout; nothing here executes on the host.
//!
//! "Latest available" resolution is best-effort: it reads the source repo's
//! `main` HEAD (derived from the pinned image's `ghcr.io/<org>/<repo>/…` path)
//! and resolves the `sha-<HEAD>` tag of each image to a digest. Any failure
//! (App not installed on the source org, GHCR hiccup) degrades to
//! `latest: null` + `check_error` rather than failing the whole page.

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use base64::Engine;
use majnet_common::platform::{ControlPlanePin, VersionFile};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::AppState;

type ApiError = (StatusCode, String);
fn bad_gateway(e: anyhow::Error) -> ApiError {
    (StatusCode::BAD_GATEWAY, format!("{e:#}"))
}
fn forbidden(e: anyhow::Error) -> ApiError {
    (StatusCode::FORBIDDEN, format!("{e:#}"))
}

/// A control-plane version pin, as the dashboard sees it.
#[derive(Debug, Clone, Serialize)]
pub struct Pin {
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub image: Option<String>,
    pub dashboard: Option<String>,
}

impl From<ControlPlanePin> for Pin {
    fn from(p: ControlPlanePin) -> Self {
        Pin {
            git_ref: p.git_ref,
            image: p.image,
            dashboard: p.dashboard,
        }
    }
}

/// One source commit between the running pin and the latest build.
#[derive(Debug, Clone, Serialize)]
pub struct Commit {
    pub sha: String,
    pub message: String,
    pub author: String,
    pub date: String,
}

/// One past commit to `version.yaml` — a rollback target.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    /// The platform-repo commit sha (the rollback handle).
    pub sha: String,
    pub message: String,
    pub author: String,
    pub date: String,
    /// The head pin (this is the running version).
    pub current: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Source {
    pub org: String,
    pub repo: String,
    pub compare_url: Option<String>,
}

/// What the control plane reports it is actually running — the bot's own build
/// metadata, CI-baked into the image (`MAJNET_BUILD_*`). bot + reconciler share
/// the image, so this one commit describes both. `None` fields on a build made
/// before the metadata was baked in.
#[derive(Debug, Clone, Serialize)]
pub struct Running {
    pub version: Option<String>,
    pub commit: Option<String>,
    pub build_time: Option<String>,
}

impl Running {
    fn from_env() -> Self {
        // Treat the Dockerfile placeholders as "unknown".
        fn var(key: &str) -> Option<String> {
            std::env::var(key)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty() && v != "unknown" && v != "dev")
        }
        Running {
            version: var("MAJNET_BUILD_VERSION"),
            commit: var("MAJNET_BUILD_COMMIT"),
            build_time: var("MAJNET_BUILD_TIME"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub current: Pin,
    pub latest: Option<Pin>,
    pub up_to_date: bool,
    pub commits: Vec<Commit>,
    pub history: Vec<HistoryEntry>,
    pub source: Source,
    /// What's actually running right now (the bot's own build).
    pub running: Running,
    /// Whether the running build matches the pinned ref. `None` when the running
    /// commit is unknown (pre-metadata build) so the UI can't tell.
    pub converged: Option<bool>,
    /// Why `latest` couldn't be resolved, if it couldn't.
    pub check_error: Option<String>,
}

/// `GET /api/control-plane` (platform-admin) — the running pin, the latest
/// available build, the commits between them, and the pin history.
pub async fn status_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Status>, ApiError> {
    crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(forbidden)?;
    do_status(&state).await.map(Json).map_err(bad_gateway)
}

async fn do_status(state: &AppState) -> Result<Status> {
    let org = &state.config.root_org;
    let client = state.github.org_client(org).await?;
    let yaml = crate::platform_api::read_platform_file(&client, org, "version.yaml").await?;
    let current: Pin = VersionFile::parse(yaml.as_bytes())
        .context("parsing version.yaml")?
        .control_plane
        .into();

    // The images pin the source: ghcr.io/<org>/<repo>/control-plane@… .
    let (src_org, src_repo) = source_from_image(current.image.as_deref())
        .unwrap_or_else(|| ("majnet".into(), "majnet".into()));

    // Best-effort latest + commits — never fail the page over it.
    let mut latest = None;
    let mut commits = Vec::new();
    let mut compare_url = None;
    let mut check_error = None;
    match resolve_latest(state, &src_org, &src_repo).await {
        Ok(l) => {
            let base = current.git_ref.trim();
            if !base.is_empty() && base != l.git_ref {
                match compare_commits(state, &src_org, &src_repo, base, &l.git_ref).await {
                    Ok((cs, url)) => {
                        commits = cs;
                        compare_url = Some(url);
                    }
                    Err(e) => tracing::warn!("control-plane compare failed: {e:#}"),
                }
            }
            latest = Some(l);
        }
        Err(e) => {
            tracing::warn!("control-plane latest check failed: {e:#}");
            check_error = Some(format!("{e:#}"));
        }
    }

    let up_to_date = match &latest {
        Some(l) => l.image == current.image && l.dashboard == current.dashboard,
        None => false,
    };

    let history = pin_history(&client, org).await.unwrap_or_else(|e| {
        tracing::warn!("control-plane pin history failed: {e:#}");
        Vec::new()
    });

    // Running-vs-pinned: the honest "converged / still rolling" signal.
    let running = Running::from_env();
    let converged = running
        .commit
        .as_deref()
        .map(|rc| commit_eq(rc, current.git_ref.trim()));

    Ok(Status {
        current,
        latest,
        up_to_date,
        commits,
        history,
        source: Source {
            org: src_org,
            repo: src_repo,
            compare_url,
        },
        running,
        converged,
        check_error,
    })
}

/// Two commit refs describe the same commit if either is a prefix of the other
/// (a full sha vs a short sha), compared on ≥7 chars.
fn commit_eq(a: &str, b: &str) -> bool {
    let n = 7.min(a.len()).min(b.len());
    n >= 7 && a[..n].eq_ignore_ascii_case(&b[..n])
}

/// Resolve the latest main build: the source `main` HEAD + each image's
/// `sha-<HEAD>` digest.
async fn resolve_latest(state: &AppState, src_org: &str, src_repo: &str) -> Result<Pin> {
    let client = state.github.org_client(src_org).await?;
    let head =
        crate::git::get_branch_head(&client, &format!("/repos/{src_org}/{src_repo}"), "main")
            .await?
            .context("source repo has no main branch")?;
    let (user, pass) = crate::proxy::ghcr_credential(state, src_org).await?;
    let tag = format!("sha-{head}");
    let cp_name = format!("{src_repo}/control-plane");
    let dash_name = format!("{src_repo}/dashboard");
    let image = crate::registry::resolve_digest(&state.http, src_org, &cp_name, &tag, &user, &pass)
        .await
        .context("resolving control-plane digest")?;
    let dashboard =
        crate::registry::resolve_digest(&state.http, src_org, &dash_name, &tag, &user, &pass)
            .await
            .context("resolving dashboard digest")?;
    Ok(Pin {
        git_ref: head,
        image: Some(image),
        dashboard: Some(dashboard),
    })
}

/// The source commits in `base..head` (newest first, capped), plus a GitHub
/// compare URL.
async fn compare_commits(
    state: &AppState,
    org: &str,
    repo: &str,
    base: &str,
    head: &str,
) -> Result<(Vec<Commit>, String)> {
    let client = state.github.org_client(org).await?;
    let v: serde_json::Value = client
        .get(
            format!("/repos/{org}/{repo}/compare/{base}...{head}"),
            None::<&()>,
        )
        .await?;
    let commits = v["commits"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .rev()
                .take(20)
                .map(|c| Commit {
                    sha: short_sha(c["sha"].as_str().unwrap_or_default()),
                    message: first_line(c["commit"]["message"].as_str().unwrap_or_default()),
                    author: c["commit"]["author"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    date: c["commit"]["author"]["date"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let url = format!(
        "https://github.com/{org}/{repo}/compare/{}...{}",
        short_sha(base),
        short_sha(head)
    );
    Ok((commits, url))
}

/// Recent commits to `version.yaml` on the platform repo — the rollback targets.
async fn pin_history(client: &octocrab::Octocrab, org: &str) -> Result<Vec<HistoryEntry>> {
    let v: serde_json::Value = client
        .get(
            format!("/repos/{org}/platform/commits?path=version.yaml&per_page=8"),
            None::<&()>,
        )
        .await?;
    Ok(v.as_array()
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, c)| HistoryEntry {
                    sha: c["sha"].as_str().unwrap_or_default().to_string(),
                    message: first_line(c["commit"]["message"].as_str().unwrap_or_default()),
                    author: c["commit"]["author"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    date: c["commit"]["author"]["date"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    current: i == 0,
                })
                .collect()
        })
        .unwrap_or_default())
}

// ── set the pin (publish / rollback) ─────────────────────────────────────────

#[derive(Deserialize)]
pub struct PinRequest {
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub dashboard: Option<String>,
    /// Rollback: copy the whole pin from this platform-repo commit instead of
    /// the explicit fields above.
    #[serde(default)]
    pub from_commit: Option<String>,
}

/// `PUT /api/control-plane/pin` (platform-admin) — commit a new pin to
/// `version.yaml`. Either explicit `{ref,image,dashboard}` (publish) or
/// `{from_commit}` (roll back to a past pin).
pub async fn pin_put(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<PinRequest>,
) -> Result<String, ApiError> {
    let actor = crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(forbidden)?;
    do_set_pin(&state, req, &actor).await.map_err(bad_gateway)
}

async fn do_set_pin(state: &AppState, req: PinRequest, actor: &str) -> Result<String> {
    let org = &state.config.root_org;
    let client = state.github.org_client(org).await?;

    // Build the target pin — from a historical commit (rollback) or explicit.
    let target = if let Some(sha) = req.from_commit.as_deref() {
        read_version_at(&client, org, sha)
            .await
            .with_context(|| format!("reading version.yaml @ {sha}"))?
    } else {
        let git_ref = req
            .git_ref
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("ref is required")?
            .to_string();
        // Invariant: control-plane images pin by digest, never by tag.
        for img in [req.image.as_deref(), req.dashboard.as_deref()]
            .into_iter()
            .flatten()
        {
            anyhow::ensure!(
                img.contains("@sha256:"),
                "images must be digest-pinned (…@sha256:…), got: {img}"
            );
        }
        ControlPlanePin {
            git_ref,
            image: req.image,
            dashboard: req.dashboard,
        }
    };

    // Read-modify-write version.yaml on platform main (same shape as upsert_node).
    let repos = client.repos(org, "platform");
    let content = repos
        .get_content()
        .path("version.yaml")
        .r#ref("main")
        .send()
        .await
        .context("reading version.yaml")?;
    let item = content
        .items
        .into_iter()
        .next()
        .context("empty contents response")?;
    let sha = item.sha;
    let current_yaml = decode_content(item.content.as_deref())?;

    let updated = format!(
        "# Managed by the platform — control-plane pin (ADR 0005/0008).\n{}",
        serde_yaml::to_string(&VersionFile {
            control_plane: target.clone(),
        })?
    );
    if updated == current_yaml {
        return Ok("Control plane is already at this pin — nothing to do.".into());
    }

    let label = pin_label(&target);
    repos
        .update_file(
            "version.yaml",
            format!("chore(control-plane): pin to {label}"),
            &updated,
            &sha,
        )
        .branch("main")
        .send()
        .await
        .context("committing version.yaml")?;
    state.store.log_event(
        "control-plane-pin",
        Some(org),
        &format!("{label} by {actor}"),
    )?;
    tracing::info!(%label, %actor, "control-plane pin updated");
    Ok(format!(
        "Pinned control plane to {label}. master-1 will roll it out within ~1 min."
    ))
}

async fn read_version_at(
    client: &octocrab::Octocrab,
    org: &str,
    r#ref: &str,
) -> Result<ControlPlanePin> {
    let content = client
        .repos(org, "platform")
        .get_content()
        .path("version.yaml")
        .r#ref(r#ref)
        .send()
        .await?;
    let item = content
        .items
        .into_iter()
        .next()
        .context("empty contents response")?;
    let yaml = decode_content(item.content.as_deref())?;
    Ok(VersionFile::parse(yaml.as_bytes())?.control_plane)
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn decode_content(content: Option<&str>) -> Result<String> {
    let encoded = content.unwrap_or_default().replace(['\n', ' '], "");
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    Ok(String::from_utf8(decoded)?)
}

/// `ghcr.io/<org>/<repo>/<component>[@…|:…]` → `(org, repo)`. The repo is
/// everything between the org and the trailing component (`control-plane`),
/// supporting multi-segment names.
fn source_from_image(image: Option<&str>) -> Option<(String, String)> {
    let image = image?;
    let path = image.strip_prefix("ghcr.io/")?;
    // Drop any @digest or :tag suffix.
    let path = path.split(['@', ':']).next().unwrap_or(path);
    let segs: Vec<&str> = path.split('/').collect();
    // Need at least org / repo / component.
    if segs.len() < 3 {
        return None;
    }
    let org = segs[0].to_string();
    let repo = segs[1..segs.len() - 1].join("/");
    Some((org, repo))
}

fn short_sha(s: &str) -> String {
    s.chars().take(7).collect()
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().trim().to_string()
}

fn short_digest(image: &str) -> Option<String> {
    let d = image.rsplit_once("@sha256:")?.1;
    Some(d.chars().take(7).collect())
}

/// A compact human label for a pin: short ref plus short image digests.
fn pin_label(p: &ControlPlanePin) -> String {
    let mut parts = vec![short_sha(&p.git_ref)];
    if let Some(cp) = p.image.as_deref().and_then(short_digest) {
        parts.push(format!("cp {cp}"));
    }
    if let Some(d) = p.dashboard.as_deref().and_then(short_digest) {
        parts.push(format!("dash {d}"));
    }
    if parts.len() == 1 {
        parts.remove(0)
    } else {
        format!("{} ({})", parts[0], parts[1..].join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_source_org_repo_from_image() {
        assert_eq!(
            source_from_image(Some("ghcr.io/majnet/majnet/control-plane@sha256:abc")),
            Some(("majnet".into(), "majnet".into()))
        );
        assert_eq!(
            source_from_image(Some("ghcr.io/acme/platform/control-plane:latest")),
            Some(("acme".into(), "platform".into()))
        );
        // Multi-segment repo.
        assert_eq!(
            source_from_image(Some("ghcr.io/org/team/repo/control-plane@sha256:x")),
            Some(("org".into(), "team/repo".into()))
        );
        assert_eq!(source_from_image(None), None);
        assert_eq!(source_from_image(Some("docker.io/x/y")), None);
    }

    #[test]
    fn pin_label_is_compact() {
        let p = ControlPlanePin {
            git_ref: "a1b2c3d4e5f6".into(),
            image: Some("ghcr.io/majnet/majnet/control-plane@sha256:7f3a9e2abcdef".into()),
            dashboard: Some("ghcr.io/majnet/majnet/dashboard@sha256:2c5d81a99999".into()),
        };
        assert_eq!(pin_label(&p), "a1b2c3d (cp 7f3a9e2, dash 2c5d81a)");
    }

    #[test]
    fn commit_eq_matches_short_and_full() {
        assert!(commit_eq(
            "f31e9b6c86b44d18501ee02b00ae451ad9d5ae8e",
            "f31e9b6"
        ));
        assert!(commit_eq("f31e9b6", "f31e9b6c86b44d18501ee02b"));
        assert!(commit_eq("ABCDEF1234", "abcdef1"));
        assert!(!commit_eq("f31e9b6", "81867c8"));
        // Too short to be confident.
        assert!(!commit_eq("f31e9", "f31e9"));
    }

    #[test]
    fn pin_label_ref_only() {
        let p = ControlPlanePin {
            git_ref: "abcdef1234".into(),
            image: None,
            dashboard: None,
        };
        assert_eq!(pin_label(&p), "abcdef1");
    }
}
