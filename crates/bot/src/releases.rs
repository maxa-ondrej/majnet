//! Releases (ADR 0009): a **release is a `vX.Y.Z`-tagged image publish**. The
//! app's CI builds + pushes `ghcr.io/<org>/<app>:vX.Y.Z` by digest; the
//! `registry_package` webhook (which already drives the testing/ephemeral
//! digest bumps) carries the tag + digest, and the bot records it here. There
//! is no separate release descriptor — the digest comes off the webhook and the
//! migration lives in the ops overlay (`base.yaml`), next to the DB/secret
//! config it depends on. `stable` auto-tracks the latest tag; `promote` pins a
//! chosen version into `production.yaml`.
//!
//! Cuts are review-gated: rather than tag on every push, the bot prepares a
//! **draft release** (proposed version + generated changelog) that refreshes on
//! each push to the app repo's `main` and waits for an operator to submit it —
//! submitting runs the same tag→CI→record flow. See the draft section below.

use anyhow::{Context, Result};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use majnet_common::project::Role;
use std::sync::Arc;

use crate::state::StoredRelease;
use crate::AppState;

type ApiError = (StatusCode, String);

// ── shared versioning: platform-cut releases (semver) ─────────────────────────
// One consistent scheme for every app: the bot computes the next semver from the
// last recorded release and creates the `vX.Y.Z` tag (App-signed, through git),
// which triggers app-release.yaml → build → `record` → then `promote`. The bump
// is chosen explicitly (patch|minor|major) or derived from conventional-commit
// messages since the last release (`auto`, option 2) — see `classify_bump`.

type Ver = (u64, u64, u64);

fn parse_semver(tag: &str) -> Option<Ver> {
    let core = tag.strip_prefix('v')?.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let x = it.next()?.parse().ok()?;
    let y = it.next()?.parse().ok()?;
    let z = it.next()?.parse().ok()?;
    it.next().is_none().then_some((x, y, z))
}

/// The next version string for a `bump` over the highest recorded release
/// (`None` = first release). Returns `X.Y.Z` (no `v` prefix).
fn next_version(last: Option<Ver>, bump: &str) -> Result<String> {
    let (x, y, z) = last.unwrap_or((0, 0, 0));
    let (x, y, z) = match bump {
        "major" => (x + 1, 0, 0),
        "minor" => (x, y + 1, 0),
        "patch" => (x, y, z + 1),
        other => anyhow::bail!("bump must be patch|minor|major, got '{other}'"),
    };
    Ok(format!("{x}.{y}.{z}"))
}

/// Derive a semver bump from conventional-commit messages (option 2). Takes the
/// strongest signal across all commits: a breaking change (`type!:` header or a
/// `BREAKING CHANGE` footer) → major; any `feat` → minor; otherwise patch.
fn classify_bump(messages: &[String]) -> &'static str {
    let mut bump = "patch";
    for m in messages {
        let header = m.lines().next().unwrap_or("");
        let (typ_scope, _) = header.split_once(':').unwrap_or((header, ""));
        let breaking = m.contains("BREAKING CHANGE") || typ_scope.trim_end().ends_with('!');
        if breaking {
            return "major";
        }
        let typ = typ_scope.split('(').next().unwrap_or("").trim();
        if typ == "feat" {
            bump = "minor";
        }
    }
    bump
}

#[derive(serde::Deserialize)]
struct Compare {
    commits: Vec<CompareCommit>,
}
#[derive(serde::Deserialize)]
struct CompareCommit {
    commit: CommitDetail,
}
#[derive(serde::Deserialize)]
struct CommitDetail {
    message: String,
}

/// The GitHub repo hosting `app` (its own name unless it's a monorepo member).
/// Best-effort via `project.yaml`; falls back to the app name.
pub(crate) async fn app_repo(state: &AppState, org: &str, app: &str) -> String {
    match crate::dashboard_api::read_project(state, org).await {
        Ok(p) => p
            .apps
            .iter()
            .find(|a| a.name == app)
            .map(|a| a.repo().to_string())
            .unwrap_or_else(|| app.to_string()),
        Err(_) => app.to_string(),
    }
}

/// The app's GHCR package path (as the packages REST API names it) and its image
/// base. A solo app is `<app>` / `ghcr.io/<org>/<app>`; a monorepo member (ADR
/// 0018) nests as `<repo>/<leaf>` / `ghcr.io/<org>/<repo>/<leaf>` — the app name
/// carries a `<repo>-` prefix but the package/image drop it. Best-effort via
/// `project.yaml`; falls back to the flat form.
pub(crate) async fn app_package(state: &AppState, org: &str, app: &str) -> (String, String) {
    match crate::dashboard_api::read_project(state, org).await {
        Ok(p) => match p.apps.iter().find(|a| a.name == app) {
            Some(decl) if decl.is_monorepo() => (
                format!("{}/{}", decl.repo(), decl.image_leaf()),
                decl.image_base(org),
            ),
            Some(decl) => (app.to_string(), decl.image_base(org)),
            None => (app.to_string(), format!("ghcr.io/{org}/{app}")),
        },
        Err(_) => (app.to_string(), format!("ghcr.io/{org}/{app}")),
    }
}

/// Commit messages on `main` that aren't reachable from `base_tag`, via the
/// GitHub compare API — the input to `classify_bump`.
async fn commits_since(
    state: &AppState,
    org: &str,
    repo: &str,
    base_tag: &str,
) -> Result<Vec<String>> {
    let client = state.github.org_client(org).await?;
    let cmp: Compare = client
        .get(
            format!("/repos/{org}/{repo}/compare/{base_tag}...main"),
            None::<&()>,
        )
        .await
        .with_context(|| format!("comparing {base_tag}...main"))?;
    Ok(cmp.commits.into_iter().map(|c| c.commit.message).collect())
}

#[derive(serde::Deserialize)]
pub struct CutQuery {
    #[serde(default = "default_bump")]
    pub bump: String,
}
fn default_bump() -> String {
    "patch".into()
}

/// `POST /api/releases/{org}/{app}/cut?bump=patch|minor|major|auto` — cut a
/// release: compute the next semver from the last recorded release and create
/// the `vX.Y.Z` tag on the app repo's `main` HEAD. `auto` derives the bump from
/// the conventional-commit messages since the last release. CI (app-release.yaml)
/// builds it, the package webhook records it, then it can be promoted.
/// Project-admin gated.
pub async fn cut(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
    Query(q): Query<CutQuery>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    let actor = crate::authz::require(&state, &headers, &org, Role::Admin)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    do_cut(&state, &org, &app, &q.bump, &actor)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

/// The apps sharing `repo` and the highest release recorded across them (the
/// repo-wide "last version"). For a solo app this is just that app; for a
/// monorepo it spans every app in the repo, since the release is repo-wide.
async fn repo_apps_and_last(state: &AppState, org: &str, repo: &str) -> (Vec<String>, Option<Ver>) {
    let repo_apps: Vec<String> = crate::dashboard_api::read_project(state, org)
        .await
        .map(|p| {
            p.apps
                .iter()
                .filter(|a| a.repo() == repo)
                .map(|a| a.name.clone())
                .collect()
        })
        .unwrap_or_default();
    let repo_apps = if repo_apps.is_empty() {
        vec![repo.to_string()]
    } else {
        repo_apps
    };
    let last = repo_apps
        .iter()
        .filter_map(|a| state.store.releases(org, a).ok())
        .flatten()
        .filter_map(|r| parse_semver(&r.version))
        .max();
    (repo_apps, last)
}

/// Create a lightweight `tag` at the repo's `main` HEAD (the release trigger).
async fn create_release_tag(state: &AppState, org: &str, repo: &str, tag: &str) -> Result<()> {
    let client = state.github.org_client(org).await?;
    let repo_path = format!("/repos/{org}/{repo}");
    let head = crate::git::get_branch_head(&client, &repo_path, "main")
        .await?
        .with_context(|| format!("repo {org}/{repo} has no main branch"))?;
    let _: serde_json::Value = client
        .post(
            format!("{repo_path}/git/refs"),
            Some(&serde_json::json!({ "ref": format!("refs/tags/{tag}"), "sha": head })),
        )
        .await
        .with_context(|| format!("creating tag {tag} (does it already exist?)"))?;
    Ok(())
}

async fn do_cut(state: &AppState, org: &str, app: &str, bump: &str, actor: &str) -> Result<String> {
    // The tag lives on the app's repo. For a monorepo the tag is repo-wide (one
    // version line shared by every app in it), so both the "last version" and
    // the commit range are computed over the repo, not the single app.
    let repo = app_repo(state, org, app).await;
    let monorepo = repo != app;
    let (_repo_apps, last) = repo_apps_and_last(state, org, &repo).await;

    // Resolve `auto` to a concrete bump from conventional commits since the last
    // release; explicit bumps pass through unchanged.
    let (effective, note) = if bump == "auto" {
        match last {
            Some((x, y, z)) => {
                let base = format!("v{x}.{y}.{z}");
                let msgs = commits_since(state, org, &repo, &base).await?;
                anyhow::ensure!(
                    !msgs.is_empty(),
                    "no new commits since {base} — nothing to release"
                );
                let b = classify_bump(&msgs);
                (
                    b.to_string(),
                    format!(" (auto → {b} from {} commits)", msgs.len()),
                )
            }
            None => ("patch".to_string(), " (auto → first release)".to_string()),
        }
    } else {
        (bump.to_string(), String::new())
    };

    let next = format!("v{}", next_version(last, &effective)?);
    create_release_tag(state, org, &repo, &next).await?;
    state.store.log_event(
        "release-cut",
        Some(org),
        &format!("{app} {next} by {actor}"),
    )?;
    tracing::info!(org, app, %repo, %next, actor, "cut release");
    let scope = if monorepo {
        format!(" on {repo} (releases every app in the monorepo)")
    } else {
        String::new()
    };
    Ok(format!(
        "Cut {next}{note}{scope} — CI is building it; it'll appear in Releases, then Promote to production."
    ))
}

// ── draft releases: review-gated cuts ─────────────────────────────────────────
// Rather than cut on every push, the bot prepares a *draft* — the proposed next
// version and a generated changelog — and waits for an operator to submit it.
// The draft refreshes on each push to the app repo's `main`; submitting runs the
// same cut→CI→record flow. Keyed per repo (a monorepo's release is repo-wide).

use crate::state::ReleaseDraft;

/// A markdown changelog from conventional-commit subjects, grouped by type.
/// Unrecognized messages land under "Other changes"; merge commits are skipped.
fn generate_changelog(messages: &[String]) -> String {
    let (mut breaking, mut feats, mut fixes, mut other) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for m in messages {
        let subject = m.lines().next().unwrap_or("").trim();
        if subject.is_empty() || subject.starts_with("Merge ") {
            continue;
        }
        // `type(scope)!: description` → (type_scope, description).
        let (typ_scope, desc) = match subject.split_once(':') {
            Some((ts, d)) => (ts.trim(), d.trim().to_string()),
            None => ("", subject.to_string()),
        };
        let is_breaking = m.contains("BREAKING CHANGE") || typ_scope.ends_with('!');
        let typ = typ_scope
            .split('(')
            .next()
            .unwrap_or("")
            .trim_end_matches('!')
            .trim();
        let line = format!("- {desc}");
        if is_breaking {
            breaking.push(line);
        } else if typ == "feat" {
            feats.push(line);
        } else if typ == "fix" {
            fixes.push(line);
        } else {
            other.push(line);
        }
    }
    let mut out = String::new();
    changelog_section("⚠️ Breaking changes", &breaking, &mut out);
    changelog_section("🚀 Features", &feats, &mut out);
    changelog_section("🐛 Fixes", &fixes, &mut out);
    changelog_section("Other changes", &other, &mut out);
    if out.is_empty() {
        out.push_str("_No notable changes._\n");
    }
    out
}

fn changelog_section(title: &str, items: &[String], out: &mut String) {
    if items.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str("## ");
    out.push_str(title);
    out.push('\n');
    out.push_str(&items.join("\n"));
    out.push('\n');
}

/// Prepare (or refresh) a repo's draft from conventional commits since its last
/// release. No unreleased commits → the draft is cleared. The store keeps
/// operator-edited notes across a refresh.
pub(crate) async fn prepare_draft(state: &AppState, org: &str, repo: &str) -> Result<()> {
    let (_apps, last) = repo_apps_and_last(state, org, repo).await;
    let (version, bump, base, count, notes) = match last {
        Some((x, y, z)) => {
            let base = format!("v{x}.{y}.{z}");
            let msgs = commits_since(state, org, repo, &base).await?;
            if msgs.is_empty() {
                state.store.delete_release_draft(org, repo)?;
                return Ok(());
            }
            let bump = classify_bump(&msgs);
            let version = format!("v{}", next_version(Some((x, y, z)), bump)?);
            let notes = generate_changelog(&msgs);
            (version, bump.to_string(), base, msgs.len() as u32, notes)
        }
        // No release yet: a first-release draft (no base to diff a changelog from).
        None => (
            "v0.0.1".to_string(),
            "patch".to_string(),
            String::new(),
            0,
            "_Initial release._\n".to_string(),
        ),
    };
    let draft = ReleaseDraft {
        repo: repo.to_string(),
        version,
        bump,
        base,
        commit_count: count,
        notes,
        notes_edited: false,
        updated_at: String::new(),
    };
    state.store.upsert_release_draft(org, &draft)?;
    tracing::info!(org, repo, version = %draft.version, count, "release draft prepared");
    Ok(())
}

/// Best-effort draft refresh on a push to an app repo's `main` (webhook entry).
/// Only declared app repos get a draft; anything else is a no-op. Errors are
/// swallowed — a draft is advisory and must never break the push flow.
pub(crate) async fn on_app_main_push(state: &AppState, org: &str, repo: &str) {
    let is_app_repo = crate::dashboard_api::read_project(state, org)
        .await
        .map(|p| p.apps.iter().any(|a| a.repo() == repo))
        .unwrap_or(false);
    if !is_app_repo {
        return;
    }
    if let Err(e) = prepare_draft(state, org, repo).await {
        tracing::warn!(
            org,
            repo,
            error = format!("{e:#}"),
            "release draft refresh failed"
        );
    }
}

/// `GET /api/releases/{org}/{app}/draft` — the pending draft for the app's repo
/// (`null` when none). Read-only, like `list`.
pub async fn draft_get(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
) -> Result<Json<Option<ReleaseDraft>>, ApiError> {
    let repo = app_repo(&state, &org, &app).await;
    state
        .store
        .release_draft(&org, &repo)
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// `POST /api/releases/{org}/{app}/draft/refresh` — recompute the draft now
/// (the push webhook does this automatically). Developer-gated.
pub async fn draft_refresh(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    crate::authz::require(&state, &headers, &org, Role::Developer)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let repo = app_repo(&state, &org, &app).await;
    prepare_draft(&state, &org, &repo)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    let draft = state
        .store
        .release_draft(&org, &repo)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(match draft {
        Some(d) => format!(
            "draft refreshed: {} ({}, {} commit(s))",
            d.version, d.bump, d.commit_count
        ),
        None => "no unreleased commits — nothing to draft".to_string(),
    })
}

#[derive(serde::Deserialize)]
pub struct NotesReq {
    pub notes: String,
}

/// `PUT /api/releases/{org}/{app}/draft/notes` — save operator-edited changelog
/// notes (kept across push refreshes). Developer-gated.
pub async fn draft_notes_put(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<NotesReq>,
) -> Result<String, ApiError> {
    crate::authz::require(&state, &headers, &org, Role::Developer)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let repo = app_repo(&state, &org, &app).await;
    let saved = state
        .store
        .set_release_draft_notes(&org, &repo, &req.notes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if saved {
        Ok("notes saved".into())
    } else {
        Err((StatusCode::NOT_FOUND, "no draft to edit".into()))
    }
}

/// `DELETE /api/releases/{org}/{app}/draft` — discard the pending draft (it
/// re-prepares on the next push). Developer-gated.
pub async fn draft_discard(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    crate::authz::require(&state, &headers, &org, Role::Developer)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let repo = app_repo(&state, &org, &app).await;
    state
        .store
        .delete_release_draft(&org, &repo)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok("draft discarded".into())
}

/// `POST /api/releases/{org}/{app}/draft/submit` — cut the pending draft: tag
/// the repo at `main` HEAD with the draft's version, persist its changelog for
/// every app in the repo, and clear the draft. CI builds the tag and the release
/// is recorded off the package webhook (same as a manual cut). Admin-gated.
pub async fn draft_submit(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    let actor = crate::authz::require(&state, &headers, &org, Role::Admin)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let repo = app_repo(&state, &org, &app).await;
    let draft = state
        .store
        .release_draft(&org, &repo)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "no draft to submit".to_string()))?;
    submit_draft(&state, &org, &repo, &draft, &actor)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn submit_draft(
    state: &AppState,
    org: &str,
    repo: &str,
    draft: &ReleaseDraft,
    actor: &str,
) -> Result<String> {
    create_release_tag(state, org, repo, &draft.version).await?;
    let (apps, _last) = repo_apps_and_last(state, org, repo).await;
    for a in &apps {
        state
            .store
            .record_release_notes(org, a, &draft.version, &draft.notes, actor)?;
    }
    state.store.delete_release_draft(org, repo)?;
    state.store.log_event(
        "release-cut",
        Some(org),
        &format!("{repo} {} by {actor} (draft)", draft.version),
    )?;
    tracing::info!(org, %repo, version = %draft.version, actor, "submitted draft release");
    let scope = if apps.len() > 1 {
        format!(" (releases {} apps in {repo})", apps.len())
    } else {
        String::new()
    };
    Ok(format!(
        "Released {}{scope} — CI is building it; it'll appear in Releases, then Promote to production.",
        draft.version
    ))
}

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
    // For a monorepo app the tag lives on the shared repo, not `/repos/{org}/{app}`.
    let repo = app_repo(state, org, app).await;
    let commit: serde_json::Value = client
        .get(format!("/repos/{org}/{repo}/commits/{tag}"), None::<&()>)
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
    // The GHCR packages REST API needs `read:packages`, which the App
    // installation token lacks — use the configured GHCR PAT (the same one that
    // authenticates image pulls), via the plain REST endpoint.
    let (_, pat) = crate::proxy::ghcr_credential(state, org).await?;
    // The GHCR package name is the image path minus the org, nested for a
    // monorepo member (`<repo>/<leaf>`). In the REST path a nested name's slash
    // must be percent-encoded. The image we record must use the same nested base
    // (not the flat `<org>/<app>`), or the pin points at a package that 404s.
    let (package, image_base) = app_package(state, org, app).await;
    let package_enc = package.replace('/', "%2F");
    let mut recorded = 0;
    // Paginate defensively (cap at 10×100 versions) so a huge package can't spin
    // forever; a break on a short page ends it early.
    for page in 1..=10u32 {
        let resp = state
            .http
            .get(format!(
                "https://api.github.com/orgs/{org}/packages/container/{package_enc}/versions?per_page=100&page={page}"
            ))
            .header("Authorization", format!("Bearer {pat}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "majnet-bot")
            .send()
            .await
            .context("listing GHCR package versions")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::ensure!(
            status.is_success(),
            "listing GHCR package versions for {app} (package {package}) ({status}): {body}"
        );
        let versions: Vec<serde_json::Value> =
            serde_json::from_str(&body).context("parsing GHCR package versions")?;
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
                    let image = format!("{image_base}@{digest}");
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
    Ok(format!(
        "backfilled {n} release(s) for {app} from the registry"
    ))
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
    use super::{
        classify_bump, generate_changelog, next_version, parse_semver, production_overlay,
    };

    fn msgs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn changelog_groups_by_conventional_type() {
        let cl = generate_changelog(&msgs(&[
            "feat(api): add CSV export (#41)",
            "fix: null deref on empty query (#43)",
            "chore: bump deps",
            "refactor!: drop the v1 endpoint",
            "docs: tidy readme",
            "Merge branch 'main' into feature",
        ]));
        // Sections present in priority order; merge commit dropped.
        let breaking = cl.find("Breaking changes").unwrap();
        let feats = cl.find("Features").unwrap();
        let fixes = cl.find("Fixes").unwrap();
        let other = cl.find("Other changes").unwrap();
        assert!(breaking < feats && feats < fixes && fixes < other);
        // The `type(scope):` prefix is stripped from the displayed line.
        assert!(cl.contains("- add CSV export (#41)"));
        assert!(cl.contains("- drop the v1 endpoint"));
        assert!(cl.contains("- bump deps"));
        assert!(!cl.contains("Merge branch"));
    }

    #[test]
    fn changelog_empty_is_placeholder() {
        assert_eq!(generate_changelog(&[]), "_No notable changes._\n");
        // A lone merge commit produces no entries either.
        assert_eq!(
            generate_changelog(&msgs(&["Merge pull request #9"])),
            "_No notable changes._\n"
        );
    }

    #[test]
    fn auto_bump_from_conventional_commits() {
        // patch by default (fixes/chores only)
        assert_eq!(
            classify_bump(&msgs(&["fix: a", "chore: deps", "docs: x"])),
            "patch"
        );
        // any feat wins over patch
        assert_eq!(classify_bump(&msgs(&["fix: a", "feat(api): b"])), "minor");
        // breaking wins over everything
        assert_eq!(
            classify_bump(&msgs(&["feat: a", "refactor!: drop v1"])),
            "major"
        );
        assert_eq!(
            classify_bump(&msgs(&["fix: a\n\nBREAKING CHANGE: db reset"])),
            "major"
        );
        // `feat!:` header is breaking, not just a feature
        assert_eq!(classify_bump(&msgs(&["feat!: rewrite"])), "major");
        // empty → patch
        assert_eq!(classify_bump(&[]), "patch");
        // non-conventional messages → patch
        assert_eq!(classify_bump(&msgs(&["wip", "merge branch"])), "patch");
    }

    #[test]
    fn semver_parse_and_bump() {
        assert_eq!(parse_semver("v1.4.2"), Some((1, 4, 2)));
        assert_eq!(parse_semver("v0.0.3"), Some((0, 0, 3)));
        assert_eq!(parse_semver("v1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_semver("latest"), None);
        assert_eq!(parse_semver("v1.2"), None);
        assert_eq!(next_version(Some((0, 0, 3)), "patch").unwrap(), "0.0.4");
        assert_eq!(next_version(Some((0, 0, 3)), "minor").unwrap(), "0.1.0");
        assert_eq!(next_version(Some((1, 4, 2)), "major").unwrap(), "2.0.0");
        assert_eq!(next_version(None, "patch").unwrap(), "0.0.1");
        assert_eq!(next_version(None, "minor").unwrap(), "0.1.0");
        assert!(next_version(None, "huge").is_err());
    }

    const NEW: &str = "ghcr.io/o/a@sha256:new";

    #[test]
    fn promote_preserves_hand_managed_production_config() {
        // The drift case: ingress was hand-added to production.yaml. A promote
        // must swap only the image and keep the ingress (ADR 0013).
        let current =
            "image: ghcr.io/o/a@sha256:old\ningress:\n  host: a.example.com\n  port: 8080\n";
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
