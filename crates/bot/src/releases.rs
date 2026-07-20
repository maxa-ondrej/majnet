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
use base64::Engine;
use majnet_common::project::{default_bump_rules, AppDecl, Autorelease, Bump, Role};
use std::collections::BTreeMap;
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
    // Accept both `vX.Y.Z` and the bare `X.Y.Z` some CIs emit (e.g. changesets
    // tags releases with the raw package version, no `v`).
    let core = tag
        .strip_prefix('v')
        .unwrap_or(tag)
        .split(['-', '+'])
        .next()?;
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

/// The commit `type` of a conventional-commit subject (`feat(scope)!: …` → `feat`),
/// and whether it's a breaking change (`type!` or a `BREAKING CHANGE` footer).
fn commit_type(message: &str) -> (&str, bool) {
    let header = message.lines().next().unwrap_or("");
    let (typ_scope, _) = header.split_once(':').unwrap_or((header, ""));
    let breaking = message.contains("BREAKING CHANGE") || typ_scope.trim_end().ends_with('!');
    let typ = typ_scope
        .split('(')
        .next()
        .unwrap_or("")
        .trim_end_matches('!')
        .trim();
    (typ, breaking)
}

/// Derive a semver bump from conventional-commit messages using `rules`
/// (type → bump, ADR 0020): a breaking change (`type!` / `BREAKING CHANGE`) is
/// always major; else the strongest bump any commit's type maps to. Types absent
/// from `rules` are ignored. `None` when nothing qualifies — no releasable change.
fn classify_bump(messages: &[String], rules: &BTreeMap<String, Bump>) -> Option<&'static str> {
    let mut best: Option<Bump> = None;
    for m in messages {
        let (typ, breaking) = commit_type(m);
        if breaking {
            return Some("major");
        }
        if let Some(&b) = rules.get(typ) {
            if best.is_none_or(|cur| b.rank() > cur.rank()) {
                best = Some(b);
            }
        }
    }
    best.map(|b| b.as_str())
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

/// This app's declaration in `project.yaml`, if present. The source of the
/// per-app release policy (ADR 0020) — scope, autorelease, paths.
pub(crate) async fn app_decl(state: &AppState, org: &str, app: &str) -> Option<AppDecl> {
    crate::dashboard_api::read_project(state, org)
        .await
        .ok()
        .and_then(|p| p.apps.into_iter().find(|a| a.name == app))
}

/// The GitHub repo hosting `app` (its own name unless it's a monorepo member).
/// Best-effort via `project.yaml`; falls back to the app name.
pub(crate) async fn app_repo(state: &AppState, org: &str, app: &str) -> String {
    app_decl(state, org, app)
        .await
        .map(|a| a.repo().to_string())
        .unwrap_or_else(|| app.to_string())
}

/// The key a release draft is tracked under (ADR 0020): the app itself in
/// per-app release mode, else the shared repo (one repo-wide version line).
/// Falls back to the app name when the app isn't declared.
pub(crate) async fn release_key(state: &AppState, org: &str, app: &str) -> String {
    app_decl(state, org, app)
        .await
        .map(|a| a.release_unit().to_string())
        .unwrap_or_else(|| app.to_string())
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

/// Commit messages on `main` since the last release — the input to
/// `classify_bump` + the changelog. With no `paths`, the whole-repo diff
/// (`base...main` via the compare API). With `paths` (per-app, ADR 0020), only
/// commits that touched those paths — listed via the commits API filtered by
/// `path` and bounded by the base commit's date — so a monorepo app's changelog
/// and `auto`-bump reflect only its own changes, not the whole repo. Tries each
/// base-ref candidate (the repo's tag scheme varies) until one resolves.
async fn commits_since(
    state: &AppState,
    org: &str,
    repo: &str,
    base_tags: &[String],
    paths: &[String],
) -> Result<Vec<String>> {
    let client = state.github.org_client(org).await?;
    // Directory prefixes the commits API can filter on (empty = not filterable,
    // e.g. a leading-glob pattern — those fall through to the whole-repo diff).
    let prefixes: Vec<String> = paths
        .iter()
        .map(|p| glob_to_prefix(p))
        .filter(|p| !p.is_empty())
        .collect();
    if prefixes.is_empty() {
        return commits_since_whole_repo(&client, org, repo, base_tags).await;
    }

    let (base_sha, since) = resolve_base_commit(&client, org, repo, base_tags).await?;
    let mut msgs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for prefix in &prefixes {
        // Paginate the path-filtered commit list, newest first, since the base
        // commit's date; the base commit itself is excluded by SHA.
        for page in 1..=20u32 {
            let params: Vec<(&str, String)> = vec![
                ("sha", "main".to_string()),
                ("path", prefix.clone()),
                ("since", since.clone()),
                ("per_page", "100".to_string()),
                ("page", page.to_string()),
            ];
            let items: Vec<serde_json::Value> = client
                .get(format!("/repos/{org}/{repo}/commits"), Some(&params))
                .await
                .with_context(|| format!("listing commits under {prefix}"))?;
            let n = items.len();
            for c in &items {
                let sha = c["sha"].as_str().unwrap_or_default();
                if sha.is_empty() || sha == base_sha {
                    continue;
                }
                if seen.insert(sha.to_string()) {
                    if let Some(m) = c["commit"]["message"].as_str() {
                        msgs.push(m.to_string());
                    }
                }
            }
            if n < 100 {
                break;
            }
        }
    }
    Ok(msgs)
}

/// The whole-repo commit diff `base...main` via the compare API (the pre-ADR-0020
/// behavior), trying each candidate base ref until one resolves.
async fn commits_since_whole_repo(
    client: &octocrab::Octocrab,
    org: &str,
    repo: &str,
    base_tags: &[String],
) -> Result<Vec<String>> {
    let mut last_err = None;
    for base in base_tags {
        let enc = base.replace('@', "%40").replace('/', "%2F");
        let res: Result<Compare, _> = client
            .get(
                format!("/repos/{org}/{repo}/compare/{enc}...main"),
                None::<&()>,
            )
            .await;
        match res {
            Ok(cmp) => return Ok(cmp.commits.into_iter().map(|c| c.commit.message).collect()),
            Err(e) => last_err = Some(anyhow::Error::from(e)),
        }
    }
    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("no base ref resolved"))
        .context(format!("comparing {:?}...main", base_tags)))
}

/// Resolve the first working base ref to its (commit SHA, committer date ISO) —
/// the boundary for a path-scoped diff. The path-filtered commit list can't be
/// bounded by SHA (the base commit may not touch the path), so we bound by date.
async fn resolve_base_commit(
    client: &octocrab::Octocrab,
    org: &str,
    repo: &str,
    base_tags: &[String],
) -> Result<(String, String)> {
    let mut last_err = None;
    for base in base_tags {
        let enc = base.replace('@', "%40").replace('/', "%2F");
        let res: Result<serde_json::Value, _> = client
            .get(format!("/repos/{org}/{repo}/commits/{enc}"), None::<&()>)
            .await;
        match res {
            Ok(c) => {
                let sha = c["sha"].as_str().unwrap_or_default().to_string();
                let date = c["commit"]["committer"]["date"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                if !sha.is_empty() && !date.is_empty() {
                    return Ok((sha, date));
                }
            }
            Err(e) => last_err = Some(anyhow::Error::from(e)),
        }
    }
    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("no base ref resolved"))
        .context("resolving base commit for path-scoped diff"))
}

/// The literal directory prefix of a path glob — everything before the first glob
/// metacharacter — for the commits API `path` filter. `applications/server/**` →
/// `applications/server`; `packages/shared/**` → `packages/shared`; a pattern
/// that starts with a glob → empty (not path-filterable).
fn glob_to_prefix(glob: &str) -> String {
    let cut = glob.find(['*', '?', '[']).unwrap_or(glob.len());
    glob[..cut].trim_end_matches('/').to_string()
}

/// The paths to scope a changelog/bump diff by: an app's `release.paths`, but
/// only in per-app mode (a repo-wide unit's changelog spans the whole repo).
fn scoped_diff_paths(decl: Option<&AppDecl>) -> &[String] {
    match decl {
        Some(d) if d.is_per_app_release() => d.release_paths(),
        _ => &[],
    }
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

/// `POST /api/releases/{org}/cut-repo/{repo}?bump=…` — cut a release for every
/// app in a monorepo in one action (ADR 0020, "all apps at version"). Per-app
/// apps are each cut at their own next version + scoped tag; a repo-wide monorepo
/// is cut once (a single shared tag). Best-effort per app — a per-app failure is
/// reported, not fatal. Project-admin gated.
pub async fn cut_repo(
    State(state): State<Arc<AppState>>,
    Path((org, repo)): Path<(String, String)>,
    Query(q): Query<CutQuery>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    let actor = crate::authz::require(&state, &headers, &org, Role::Admin)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let project = crate::dashboard_api::read_project(&state, &org)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    let repo_apps: Vec<&AppDecl> = project.apps.iter().filter(|a| a.repo() == repo).collect();
    if repo_apps.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no apps declared for repo {repo} in {org}"),
        ));
    }
    // One target per release unit: each per-app app, plus (once) any repo-wide
    // app — so a repo-wide monorepo cuts a single shared tag, not one per app.
    let mut targets: Vec<String> = Vec::new();
    let mut repo_wide_rep: Option<String> = None;
    for a in &repo_apps {
        if a.is_per_app_release() {
            targets.push(a.name.clone());
        } else if repo_wide_rep.is_none() {
            repo_wide_rep = Some(a.name.clone());
        }
    }
    targets.extend(repo_wide_rep);

    let mut lines = Vec::new();
    for app in &targets {
        match do_cut(&state, &org, app, &q.bump, &actor).await {
            Ok(msg) => lines.push(format!("{app}: {msg}")),
            // "nothing to release" isn't a failure of the bulk action — an app
            // with no releasable commits is simply skipped.
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("nothing to release") {
                    lines.push(format!("{app}: skipped — nothing to release"));
                } else {
                    lines.push(format!("{app}: FAILED — {msg}"));
                }
            }
        }
    }
    Ok(lines.join("\n"))
}

/// The apps sharing `repo` and the highest release recorded across them (the
/// repo-wide "last version"). For a solo app this is just that app; for a
/// monorepo it spans every app in the repo, since the release is repo-wide.
/// The highest recorded release across a repo's apps, with the app it came from
/// and the version-prefix style to preserve (`v` if the recorded tag had one,
/// else bare) so cut/draft output matches the repo's existing convention.
struct LastRelease {
    ver: Ver,
    app: String,
    prefix: &'static str,
}

impl LastRelease {
    /// Candidate git-tag refs for this release, most-specific first, so the
    /// commit diff resolves whichever release-tag scheme the repo uses: a
    /// MajNet-cut `vX.Y.Z` / bare `X.Y.Z`, or a changesets per-package
    /// `@<repo>/<leaf>@<X.Y.Z>` for a monorepo member.
    fn tag_candidates(&self, repo: &str) -> Vec<String> {
        let (x, y, z) = self.ver;
        let core = format!("{x}.{y}.{z}");
        let mut c = vec![
            format!("{}{core}", self.prefix),
            format!("v{core}"),
            core.clone(),
        ];
        if self.app != repo {
            let leaf = self
                .app
                .strip_prefix(&format!("{repo}-"))
                .unwrap_or(&self.app);
            c.push(format!("@{repo}/{leaf}@{core}"));
        }
        c.dedup();
        c
    }

    /// The last version rendered in the repo's own prefix style (for display).
    fn display(&self) -> String {
        let (x, y, z) = self.ver;
        format!("{}{x}.{y}.{z}", self.prefix)
    }
}

/// The highest recorded release across `apps` — with the app it came from and
/// the version-prefix style to preserve (`v` if the recorded tag had one, else
/// bare). `None` when none of them has a release yet.
fn highest_release(state: &AppState, org: &str, apps: &[String]) -> Option<LastRelease> {
    let mut last: Option<LastRelease> = None;
    for app in apps {
        for r in state.store.releases(org, app).into_iter().flatten() {
            if let Some(ver) = parse_semver(&r.version) {
                if last.as_ref().is_none_or(|l| ver > l.ver) {
                    let prefix = if r.version.starts_with('v') { "v" } else { "" };
                    last = Some(LastRelease {
                        ver,
                        app: app.clone(),
                        prefix,
                    });
                }
            }
        }
    }
    last
}

async fn repo_apps_and_last(
    state: &AppState,
    org: &str,
    repo: &str,
) -> (Vec<String>, Option<LastRelease>) {
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
    let last = highest_release(state, org, &repo_apps);
    (repo_apps, last)
}

/// The apps + highest release for an app's *release unit* (ADR 0020): just this
/// app in per-app mode (each app releases independently), or every app sharing
/// the repo in repo-wide mode (one shared version line). Drives the cut/draft
/// "last version" and which apps a submitted changelog is recorded against.
async fn unit_apps_and_last(
    state: &AppState,
    org: &str,
    app: &str,
) -> (Vec<String>, Option<LastRelease>) {
    match app_decl(state, org, app).await {
        Some(d) if d.is_per_app_release() => {
            let apps = vec![app.to_string()];
            let last = highest_release(state, org, &apps);
            (apps, last)
        }
        Some(d) => repo_apps_and_last(state, org, d.repo()).await,
        None => repo_apps_and_last(state, org, app).await,
    }
}

/// Both scoped-tag spellings for a per-app release at semver `core` — most
/// specific first: `@<scope>/<leaf>@vX.Y.Z` then the bare `@<scope>/<leaf>@X.Y.Z`.
/// The **git tag** may carry a `v` (Changesets tags `@<scope>/<leaf>@vX.Y.Z`)
/// while the **image tag** MajNet records is bare (`X.Y.Z`), so we can't infer
/// the git-tag prefix from the stored version — try both.
fn scoped_tag_variants(decl: &AppDecl, core: &str) -> Vec<String> {
    let core = core.trim_start_matches('v');
    vec![
        decl.release_tag(&format!("v{core}")),
        decl.release_tag(core),
    ]
}

/// The git tag to **create** when releasing `app` at `version` (ADR 0020). A
/// per-app release always uses the `v`-prefixed scoped form
/// `@<scope>/<leaf>@vX.Y.Z` — Changesets prefixes the scoped git tag with `v`
/// even though the image/recorded version is bare, so we match that convention
/// regardless of the stored version's prefix. Repo-wide releases use the version
/// verbatim (its own preserved `v`/bare prefix).
fn release_tag_to_create(decl: Option<&AppDecl>, version: &str) -> String {
    match decl {
        Some(d) if d.is_per_app_release() => {
            d.release_tag(&format!("v{}", version.trim_start_matches('v')))
        }
        _ => version.to_string(),
    }
}

/// Candidate base refs for the commit diff since `last`, most-specific first.
/// Prepends the app's *configured* per-app scoped tag (ADR 0020) in both `v` and
/// bare spellings — which `LastRelease::tag_candidates` can't know when the scope
/// differs from the repo name — then the generic candidates (`vX.Y.Z`, bare,
/// changesets-scoped).
fn base_tag_candidates(decl: Option<&AppDecl>, repo: &str, last: &LastRelease) -> Vec<String> {
    let mut c = last.tag_candidates(repo);
    if let Some(d) = decl {
        if d.is_per_app_release() {
            let (x, y, z) = last.ver;
            // Insert in reverse so the final order keeps the `v` form first.
            for t in scoped_tag_variants(d, &format!("{x}.{y}.{z}"))
                .into_iter()
                .rev()
            {
                if !c.contains(&t) {
                    c.insert(0, t);
                }
            }
        }
    }
    c
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
    // The tag lives on the app's repo. In per-app mode (ADR 0020) the tag is
    // scoped to the app (`@<scope>/<leaf>@<ver>`) and both the "last version" and
    // commit range are per-app; otherwise the tag is repo-wide (one version line
    // shared by every app in the monorepo). `unit_apps_and_last` picks the scope.
    let decl = app_decl(state, org, app).await;
    let repo = decl
        .as_ref()
        .map(|d| d.repo().to_string())
        .unwrap_or_else(|| app.to_string());
    let per_app = decl.as_ref().is_some_and(|d| d.is_per_app_release());
    let rules = decl
        .as_ref()
        .map(|d| d.bump_rules())
        .unwrap_or_else(default_bump_rules);
    let (_unit_apps, last) = unit_apps_and_last(state, org, app).await;
    // Preserve the existing version-prefix style (`v` or bare); default to `v`
    // for a brand-new app with no prior release.
    let prefix = last.as_ref().map(|l| l.prefix).unwrap_or("v");

    // Resolve `auto` to a concrete bump from conventional commits since the last
    // release; explicit bumps pass through unchanged. `msgs` (path-scoped
    // per-app) also feeds the pushed changelog. For `auto` the diff is required
    // (propagate errors); for an explicit bump it's best-effort (changelog only).
    let (effective, note, msgs) = if bump == "auto" {
        match &last {
            Some(l) => {
                let cands = base_tag_candidates(decl.as_ref(), &repo, l);
                let msgs =
                    commits_since(state, org, &repo, &cands, scoped_diff_paths(decl.as_ref()))
                        .await?;
                anyhow::ensure!(
                    !msgs.is_empty(),
                    "no new commits since {} — nothing to release",
                    l.display()
                );
                let b = classify_bump(&msgs, &rules).with_context(|| {
                    format!(
                        "no releasable (feat/fix/breaking) commits since {} — nothing to release",
                        l.display()
                    )
                })?;
                (
                    b.to_string(),
                    format!(" (auto → {b} from {} commits)", msgs.len()),
                    msgs,
                )
            }
            None => (
                "patch".to_string(),
                " (auto → first release)".to_string(),
                Vec::new(),
            ),
        }
    } else {
        let msgs = match &last {
            Some(l) => {
                let cands = base_tag_candidates(decl.as_ref(), &repo, l);
                commits_since(state, org, &repo, &cands, scoped_diff_paths(decl.as_ref()))
                    .await
                    .unwrap_or_default()
            }
            None => Vec::new(),
        };
        (bump.to_string(), String::new(), msgs)
    };

    let next = format!(
        "{prefix}{}",
        next_version(last.as_ref().map(|l| l.ver), &effective)?
    );

    // Per-app: push the version bump + changelog to the repo, then tag that
    // commit; otherwise just tag `main` HEAD. package.json takes the bare semver.
    if let Some(dir) = app_dir(decl.as_ref()) {
        let notes = if last.is_none() {
            "_Initial release._\n".to_string()
        } else {
            generate_changelog(&msgs, &rules)
        };
        let core = next.trim_start_matches('v');
        // Best-effort: a protected `main` with no App bypass rejects the direct
        // push. Don't abort the release for that — fall back to tagging `main`
        // HEAD without the in-repo bump (grant the App a ruleset bypass to enable
        // the version/changelog push there).
        if let Err(e) = push_release_commit(state, org, &repo, app, &dir, core, &notes).await {
            tracing::warn!(
                org,
                app,
                error = format!("{e:#}"),
                "release version/changelog push failed — tagging without the in-repo bump"
            );
        }
    }

    // The git tag: per-app → `@<scope>/<leaf>@vX.Y.Z`, else the plain version.
    let tag = release_tag_to_create(decl.as_ref(), &next);
    create_release_tag(state, org, &repo, &tag).await?;
    state.store.log_event(
        "release-cut",
        Some(org),
        &format!("{app} {next} by {actor}"),
    )?;
    tracing::info!(org, app, %repo, %next, %tag, actor, "cut release");
    let scope = if per_app {
        format!(" (tag {tag})")
    } else if repo != app {
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

/// A markdown changelog from conventional-commit subjects, grouped by the bump
/// level each commit resolves to under `rules` (ADR 0020): breaking → Breaking,
/// minor-level types → Features, patch-level types → Fixes. Types absent from
/// `rules` are ignored; merge commits are skipped.
fn generate_changelog(messages: &[String], rules: &BTreeMap<String, Bump>) -> String {
    let (mut breaking, mut feats, mut fixes) = (Vec::new(), Vec::new(), Vec::new());
    for m in messages {
        let subject = m.lines().next().unwrap_or("").trim();
        if subject.is_empty() || subject.starts_with("Merge ") {
            continue;
        }
        let desc = subject
            .split_once(':')
            .map(|(_, d)| d.trim())
            .unwrap_or(subject);
        let (typ, is_breaking) = commit_type(m);
        let line = format!("- {desc}");
        if is_breaking {
            breaking.push(line);
            continue;
        }
        match rules.get(typ) {
            Some(Bump::Major) => breaking.push(line),
            Some(Bump::Minor) => feats.push(line),
            Some(Bump::Patch) => fixes.push(line),
            None => {} // ignored type
        }
    }
    let mut out = String::new();
    changelog_section("⚠️ Breaking changes", &breaking, &mut out);
    changelog_section("🚀 Features", &feats, &mut out);
    changelog_section("🐛 Fixes", &fixes, &mut out);
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

// ── release file push: version bump + changelog into the repo (ADR 0020) ──────
// A per-app release commits `<app-dir>/package.json` (version bumped) + prepends
// `<app-dir>/CHANGELOG.md`, then tags that commit. The app dir is the literal
// prefix of the first `release.paths` glob. Direct push to `main` (no PR), with a
// commit-message marker so the push doesn't re-trigger autorelease.

/// Marker prefix on a MajNet release commit — `on_app_main_push` skips autorelease
/// for a push whose head commit is one, breaking the release→push→release loop.
const RELEASE_COMMIT_PREFIX: &str = "chore(release): ";

/// The app's directory in the repo — the literal prefix of its first
/// `release.paths` glob (`applications/server/**` → `applications/server`).
/// `None` unless it's a per-app release with a usable path; then the cut just
/// tags, without a version/changelog push.
fn app_dir(decl: Option<&AppDecl>) -> Option<String> {
    let d = decl?;
    if !d.is_per_app_release() {
        return None;
    }
    d.release_paths()
        .iter()
        .map(|p| glob_to_prefix(p))
        .find(|p| !p.is_empty())
}

/// Set the first `"version"` string value in package.json text, preserving
/// formatting + key order (a serde round-trip would reorder keys). Errors if the
/// key is absent/malformed.
fn set_json_version(content: &str, version: &str) -> Result<String> {
    let key = content
        .find("\"version\"")
        .context("package.json has no \"version\" key")?;
    let after = &content[key..];
    let colon = after.find(':').context("malformed \"version\" entry")?;
    let rest = &after[colon + 1..];
    let q1 = rest
        .find('"')
        .context("\"version\" value is not a string")?;
    let q2 = rest[q1 + 1..]
        .find('"')
        .context("unterminated \"version\" value")?;
    let start = key + colon + 1 + q1 + 1;
    let end = start + q2;
    Ok(format!("{}{version}{}", &content[..start], &content[end..]))
}

/// A CHANGELOG.md entry: the version as an H2, with the generated notes' H2
/// section headers demoted to H3 so they nest beneath it.
fn changelog_entry(version: &str, notes: &str) -> String {
    let body = notes
        .lines()
        .map(|l| {
            l.strip_prefix("## ")
                .map(|r| format!("### {r}"))
                .unwrap_or_else(|| l.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("## {version}\n\n{}\n", body.trim())
}

/// Prepend a changelog entry, keeping a leading `# ` title at the top.
fn prepend_changelog(existing: Option<&str>, entry: &str) -> String {
    match existing {
        Some(c) if !c.trim().is_empty() => {
            let c = c.trim_start_matches('\u{feff}');
            if let Some(rest) = c.strip_prefix("# ") {
                let (title, body) = rest.split_once('\n').unwrap_or((rest, ""));
                format!("# {}\n\n{entry}\n{}", title.trim_end(), body.trim_start())
            } else {
                format!("{entry}\n{c}")
            }
        }
        _ => format!("# Changelog\n\n{entry}"),
    }
}

/// A repo file's text on `main`, or `None` if absent/unreadable.
async fn read_repo_file(
    client: &octocrab::Octocrab,
    org: &str,
    repo: &str,
    path: &str,
) -> Option<String> {
    let content = client
        .repos(org, repo)
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

type Changes = std::collections::BTreeMap<String, Option<String>>;

/// The file changes for one app's release (bumped `<dir>/package.json` +
/// prepended `<dir>/CHANGELOG.md`), read from `main`. `version` is the bare
/// semver. Best-effort: a missing package.json is skipped (changelog still
/// written). Returned as a map so several apps' changes can be merged into one
/// commit (batch release).
async fn release_file_changes(
    client: &octocrab::Octocrab,
    org: &str,
    repo: &str,
    app: &str,
    dir: &str,
    version: &str,
    notes: &str,
) -> Changes {
    let mut changes: Changes = Default::default();
    let pkg_path = format!("{dir}/package.json");
    if let Some(pkg) = read_repo_file(client, org, repo, &pkg_path).await {
        match set_json_version(&pkg, version) {
            Ok(updated) if updated != pkg => {
                changes.insert(pkg_path, Some(updated));
            }
            Ok(_) => {} // already at this version
            Err(e) => {
                tracing::warn!(
                    org,
                    app,
                    error = format!("{e:#}"),
                    "skipping package.json bump"
                )
            }
        }
    } else {
        tracing::info!(org, app, %pkg_path, "no package.json — skipping version bump");
    }
    let cl_path = format!("{dir}/CHANGELOG.md");
    let existing = read_repo_file(client, org, repo, &cl_path).await;
    let entry = changelog_entry(version, notes);
    changes.insert(
        cl_path,
        Some(prepend_changelog(existing.as_deref(), &entry)),
    );
    changes
}

/// Commit `changes` to the repo's `main` in one commit (ADR 0020); the caller
/// then tags it. Fast-forward only — a concurrent push errors rather than
/// clobbers. No-op when `changes` is empty. The message carries
/// `RELEASE_COMMIT_PREFIX` so the push doesn't re-trigger autorelease.
async fn commit_changes_to_main(
    client: &octocrab::Octocrab,
    org: &str,
    repo: &str,
    changes: &Changes,
    message: &str,
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let repo_path = format!("/repos/{org}/{repo}");
    let head = crate::git::get_branch_head(client, &repo_path, "main")
        .await?
        .with_context(|| format!("repo {org}/{repo} has no main branch"))?;
    let base_tree = crate::git::commit_tree(client, &repo_path, &head).await?;
    let tree = crate::git::create_tree_incremental(client, &repo_path, &base_tree, changes).await?;
    let commit = crate::git::create_commit(client, &repo_path, &tree, &[&head], message).await?;
    crate::git::update_ref(client, &repo_path, "main", &commit).await?;
    Ok(())
}

/// Push one app's version bump + changelog to `main` in a single commit, then
/// the caller tags it (the single-app path; batch releases merge many apps into
/// one commit instead — see `bulk_submit`).
async fn push_release_commit(
    state: &AppState,
    org: &str,
    repo: &str,
    app: &str,
    dir: &str,
    version: &str,
    notes: &str,
) -> Result<()> {
    let client = state.github.org_client(org).await?;
    let changes = release_file_changes(&client, org, repo, app, dir, version, notes).await;
    let msg = format!("{RELEASE_COMMIT_PREFIX}{app} {version}");
    commit_changes_to_main(&client, org, repo, &changes, &msg).await?;
    tracing::info!(org, app, version, "pushed release version bump + changelog");
    Ok(())
}

/// Prepare (or refresh) the draft for `app`'s release unit (ADR 0020) from
/// conventional commits since its last release. The unit is the app itself
/// (per-app mode) or the shared repo (repo-wide); the draft is keyed by that
/// unit. No unreleased commits → the draft is cleared. Operator-edited notes
/// survive a refresh.
pub(crate) async fn prepare_draft(state: &AppState, org: &str, app: &str) -> Result<()> {
    let decl = app_decl(state, org, app).await;
    let repo = decl
        .as_ref()
        .map(|d| d.repo().to_string())
        .unwrap_or_else(|| app.to_string());
    let key = decl
        .as_ref()
        .map(|d| d.release_unit().to_string())
        .unwrap_or_else(|| app.to_string());
    let rules = decl
        .as_ref()
        .map(|d| d.bump_rules())
        .unwrap_or_else(default_bump_rules);
    let (_apps, last) = unit_apps_and_last(state, org, app).await;
    let prefix = last.as_ref().map(|l| l.prefix).unwrap_or("v");
    let (version, bump, base, count, notes) = match &last {
        Some(l) => {
            let cands = base_tag_candidates(decl.as_ref(), &repo, l);
            let msgs =
                commits_since(state, org, &repo, &cands, scoped_diff_paths(decl.as_ref())).await?;
            // No commits, or only ignored types (chore/docs/…) → no releasable
            // change, so there's no candidate: clear any stale draft.
            let Some(bump) = classify_bump(&msgs, &rules) else {
                state.store.delete_release_draft(org, &key)?;
                return Ok(());
            };
            let version = format!("{prefix}{}", next_version(Some(l.ver), bump)?);
            let notes = generate_changelog(&msgs, &rules);
            (
                version,
                bump.to_string(),
                l.display(),
                msgs.len() as u32,
                notes,
            )
        }
        // No release yet: a first-release draft (no base to diff a changelog from).
        None => (
            format!("{prefix}0.0.1"),
            "patch".to_string(),
            String::new(),
            0,
            "_Initial release._\n".to_string(),
        ),
    };
    let draft = ReleaseDraft {
        repo: key.clone(),
        version,
        bump,
        base,
        commit_count: count,
        notes,
        notes_edited: false,
        updated_at: String::new(),
    };
    state.store.upsert_release_draft(org, &draft)?;
    tracing::info!(org, %key, version = %draft.version, count, "release draft prepared");
    Ok(())
}

/// Handle a push to an app repo's `main` (webhook entry). A repo can host several
/// release units (each per-app app + one repo-wide unit for the rest); for each,
/// either **autorelease** it (ADR 0020 phase 2 — when `autorelease` is on and a
/// `changed` file matches its `paths`) or refresh its advisory **draft**. Only
/// declared app repos do anything. Errors are swallowed — a push must never
/// break, and releasing is best-effort.
///
/// `head_msg` is the push's head-commit message: a MajNet release commit (which
/// pushes the version bump + changelog) carries `RELEASE_COMMIT_PREFIX`, and we
/// **skip autorelease** for it so a release can't re-trigger another release.
pub(crate) async fn on_app_main_push(
    state: &AppState,
    org: &str,
    repo: &str,
    changed: &[String],
    head_msg: &str,
) {
    let Ok(project) = crate::dashboard_api::read_project(state, org).await else {
        return;
    };
    let is_release_commit = head_msg.starts_with(RELEASE_COMMIT_PREFIX);
    // One representative app per release unit: each per-app app, plus one for the
    // repo-wide unit (they share a single draft/line).
    let mut reps: Vec<AppDecl> = Vec::new();
    let mut repo_wide_rep: Option<AppDecl> = None;
    for a in project.apps.iter().filter(|a| a.repo() == repo) {
        if a.is_per_app_release() {
            reps.push(a.clone());
        } else if repo_wide_rep.is_none() {
            repo_wide_rep = Some(a.clone());
        }
    }
    reps.extend(repo_wide_rep);
    for rep in reps {
        // Autorelease-enabled units auto-cut on a matching change; the rest just
        // refresh their draft (mutually exclusive per unit). A release commit's
        // own push never autoreleases (loop guard).
        let result = if rep.autorelease_mode() != Autorelease::Off && !is_release_commit {
            try_autorelease(state, org, &rep, changed).await
        } else {
            prepare_draft(state, org, &rep.name).await
        };
        if let Err(e) = result {
            tracing::warn!(
                org,
                repo,
                app = %rep.name,
                error = format!("{e:#}"),
                "release refresh/autorelease failed"
            );
        }
    }
}

/// Autorelease `app` if the push changed a file under its configured `paths`
/// (ADR 0020 phase 2). Cuts the per-app release via the same tag→CI path as a
/// manual cut — `patch` always bumps patch, `auto` derives it from conventional
/// commits. A no-op when nothing matched, no `paths` are set, or there are no
/// unreleased commits (a benign `auto` case).
async fn try_autorelease(
    state: &AppState,
    org: &str,
    app: &AppDecl,
    changed: &[String],
) -> Result<()> {
    let paths = app.release_paths();
    if paths.is_empty() {
        tracing::info!(org, app = %app.name, "autorelease on but no paths set — skipping");
        return Ok(());
    }
    if !paths_match(paths, changed) {
        return Ok(()); // this app's files didn't change in this push
    }
    let bump = match app.autorelease_mode() {
        Autorelease::Patch => "patch",
        Autorelease::Auto => "auto",
        Autorelease::Off => return Ok(()),
    };
    match do_cut(state, org, &app.name, bump, "autorelease").await {
        Ok(msg) => {
            tracing::info!(org, app = %app.name, %msg, "autoreleased");
            Ok(())
        }
        // `auto` with no new commits since the last release is benign here (the
        // matching change may already be released) — don't surface it as an error.
        Err(e) if format!("{e:#}").contains("nothing to release") => {
            tracing::info!(org, app = %app.name, "autorelease: no new commits — skipping");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Whether any `files` path matches any of the `patterns` globs (gitignore-style:
/// `*` stops at `/`, `**` crosses it). Invalid globs are skipped with a warning.
fn paths_match(patterns: &[String], files: &[String]) -> bool {
    let mut builder = globset::GlobSetBuilder::new();
    for p in patterns {
        match globset::GlobBuilder::new(p).literal_separator(true).build() {
            Ok(g) => {
                builder.add(g);
            }
            Err(e) => tracing::warn!(pattern = %p, error = %e, "invalid autorelease glob"),
        }
    }
    match builder.build() {
        Ok(set) => files.iter().any(|f| set.is_match(f)),
        Err(_) => false,
    }
}

/// `GET /api/releases/{org}/{app}/draft` — the pending draft for the app's repo
/// (`null` when none). Read-only, like `list`.
/// A fleet-wide release candidate — a repo with a pending draft — for the
/// top-bar "Releases" surface. `app` is a representative member of the repo, so
/// the dashboard can deep-link to that app's Releases section.
#[derive(serde::Serialize)]
pub struct DraftSummary {
    pub org: String,
    pub app: String,
    pub repo: String,
    pub version: String,
    pub bump: String,
    pub commit_count: u32,
    pub updated_at: String,
}

/// `GET /api/releases/drafts` — every pending release candidate across all
/// projects (one row per repo; a monorepo's draft is repo-wide). Backs the
/// top-bar "Releases" popover, mirroring `/deploys`.
pub async fn drafts_all(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<DraftSummary>>, ApiError> {
    let drafts = state
        .store
        .all_release_drafts()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Group by org so each project.yaml is read once, then map every draft's
    // repo to a representative app for the deep-link (solo app: repo == app).
    let mut by_org: std::collections::BTreeMap<String, Vec<ReleaseDraft>> =
        std::collections::BTreeMap::new();
    for (org, d) in drafts {
        by_org.entry(org).or_default().push(d);
    }
    let mut out = Vec::new();
    for (org, ds) in by_org {
        let project = crate::dashboard_api::read_project(&state, &org).await.ok();
        for d in ds {
            // The draft is keyed by release unit (app name in per-app mode, else
            // the repo). Map it back to a representative app for the deep-link and
            // its real git repo for display.
            let matched = project
                .as_ref()
                .and_then(|p| p.apps.iter().find(|a| a.release_unit() == d.repo));
            let app = matched
                .map(|a| a.name.clone())
                .unwrap_or_else(|| d.repo.clone());
            let repo = matched
                .map(|a| a.repo().to_string())
                .unwrap_or_else(|| d.repo.clone());
            out.push(DraftSummary {
                org: org.clone(),
                app,
                repo,
                version: d.version,
                bump: d.bump,
                commit_count: d.commit_count,
                updated_at: d.updated_at,
            });
        }
    }
    Ok(Json(out))
}

pub async fn draft_get(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
) -> Result<Json<Option<ReleaseDraft>>, ApiError> {
    let key = release_key(&state, &org, &app).await;
    state
        .store
        .release_draft(&org, &key)
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
    prepare_draft(&state, &org, &app)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    let key = release_key(&state, &org, &app).await;
    let draft = state
        .store
        .release_draft(&org, &key)
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
    let key = release_key(&state, &org, &app).await;
    let saved = state
        .store
        .set_release_draft_notes(&org, &key, &req.notes)
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
    let key = release_key(&state, &org, &app).await;
    state
        .store
        .delete_release_draft(&org, &key)
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
    let key = release_key(&state, &org, &app).await;
    let draft = state
        .store
        .release_draft(&org, &key)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "no draft to submit".to_string()))?;
    submit_draft(&state, &org, &app, &draft, &actor)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn submit_draft(
    state: &AppState,
    org: &str,
    app: &str,
    draft: &ReleaseDraft,
    actor: &str,
) -> Result<String> {
    // Tag the app's release unit: per-app gets a scoped `@<scope>/<leaf>@<ver>`
    // tag, repo-wide the plain version. Notes attach to every app the unit covers
    // (just this app per-app; all repo apps repo-wide).
    let decl = app_decl(state, org, app).await;
    let repo = decl
        .as_ref()
        .map(|d| d.repo().to_string())
        .unwrap_or_else(|| app.to_string());
    let key = decl
        .as_ref()
        .map(|d| d.release_unit().to_string())
        .unwrap_or_else(|| app.to_string());
    // Per-app: push the version bump + changelog (the draft's notes), then tag
    // that commit; otherwise just tag `main` HEAD.
    if let Some(dir) = app_dir(decl.as_ref()) {
        let core = draft.version.trim_start_matches('v');
        push_release_commit(state, org, &repo, app, &dir, core, &draft.notes).await?;
    }
    let tag = release_tag_to_create(decl.as_ref(), &draft.version);
    create_release_tag(state, org, &repo, &tag).await?;
    let (apps, _last) = unit_apps_and_last(state, org, app).await;
    for a in &apps {
        state
            .store
            .record_release_notes(org, a, &draft.version, &draft.notes, actor)?;
    }
    state.store.delete_release_draft(org, &key)?;
    state.store.log_event(
        "release-cut",
        Some(org),
        &format!("{key} {} by {actor} (draft)", draft.version),
    )?;
    tracing::info!(org, %key, %tag, version = %draft.version, actor, "submitted draft release");
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

#[derive(serde::Deserialize)]
pub struct BulkItem {
    pub org: String,
    pub app: String,
}
#[derive(serde::Deserialize)]
pub struct BulkReq {
    pub items: Vec<BulkItem>,
}

/// `POST /api/releases/bulk` — release several candidates at once. Candidates in
/// the **same monorepo** are committed together — one version-bump + changelog
/// commit, then one tag per app on it — so sibling releases can't race the `main`
/// fast-forward (the bug when each was submitted independently). Distinct repos
/// are independent. Admin-gated per org; a per-item failure is reported, not
/// fatal.
pub async fn bulk_submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<BulkReq>,
) -> Result<String, ApiError> {
    let mut by_org: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for it in req.items {
        by_org.entry(it.org).or_default().push(it.app);
    }
    let mut lines = Vec::new();
    for (org, apps) in by_org {
        let actor = match crate::authz::require(&state, &headers, &org, Role::Admin).await {
            Ok(a) => a,
            Err(e) => {
                lines.push(format!("{org}: FORBIDDEN — {e:#}"));
                continue;
            }
        };
        // Group the org's apps by their git repo, so a monorepo's apps release in
        // one commit.
        let mut by_repo: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for app in apps {
            let repo = app_repo(&state, &org, &app).await;
            by_repo.entry(repo).or_default().push(app);
        }
        for (repo, group) in by_repo {
            match submit_repo_group(&state, &org, &repo, &group, &actor).await {
                Ok(mut ls) => lines.append(&mut ls),
                Err(e) => lines.push(format!("{repo}: FAILED — {e:#}")),
            }
        }
    }
    Ok(lines.join("\n"))
}

struct BulkTarget {
    app: String,
    decl: Option<AppDecl>,
    draft: ReleaseDraft,
}

/// Release every requested app in one repo as a single commit (merged version
/// bumps + changelogs) plus one tag per app — the `main` push happens once, so
/// siblings never race the fast-forward.
async fn submit_repo_group(
    state: &AppState,
    org: &str,
    repo: &str,
    apps: &[String],
    actor: &str,
) -> Result<Vec<String>> {
    let client = state.github.org_client(org).await?;
    let mut lines = Vec::new();
    let mut targets: Vec<BulkTarget> = Vec::new();
    for app in apps {
        let decl = app_decl(state, org, app).await;
        let key = decl
            .as_ref()
            .map(|d| d.release_unit().to_string())
            .unwrap_or_else(|| app.clone());
        match state.store.release_draft(org, &key)? {
            Some(draft) => targets.push(BulkTarget {
                app: app.clone(),
                decl,
                draft,
            }),
            None => lines.push(format!("{app}: skipped — no draft")),
        }
    }
    if targets.is_empty() {
        return Ok(lines);
    }

    // 1. Merge every app's version bump + changelog into one commit on `main`.
    let mut changes: Changes = Default::default();
    let mut labels = Vec::new();
    for t in &targets {
        if let Some(dir) = app_dir(t.decl.as_ref()) {
            let core = t.draft.version.trim_start_matches('v');
            let ch =
                release_file_changes(&client, org, repo, &t.app, &dir, core, &t.draft.notes).await;
            changes.extend(ch);
        }
        let leaf = t
            .decl
            .as_ref()
            .map(|d| d.image_leaf().to_string())
            .unwrap_or_else(|| t.app.clone());
        labels.push(format!("{leaf} {}", t.draft.version));
    }
    let msg = format!("{RELEASE_COMMIT_PREFIX}{}", labels.join(", "));
    if let Err(e) = commit_changes_to_main(&client, org, repo, &changes, &msg).await {
        tracing::warn!(
            org,
            %repo,
            error = format!("{e:#}"),
            "bulk release file push failed — tagging without the in-repo bump"
        );
    }

    // 2. Tag each app (all pointing at the one commit), record notes, clear drafts.
    for t in &targets {
        let tag = release_tag_to_create(t.decl.as_ref(), &t.draft.version);
        match create_release_tag(state, org, repo, &tag).await {
            Ok(()) => {
                let key = t
                    .decl
                    .as_ref()
                    .map(|d| d.release_unit().to_string())
                    .unwrap_or_else(|| t.app.clone());
                let (unit_apps, _last) = unit_apps_and_last(state, org, &t.app).await;
                for a in &unit_apps {
                    state.store.record_release_notes(
                        org,
                        a,
                        &t.draft.version,
                        &t.draft.notes,
                        actor,
                    )?;
                }
                state.store.delete_release_draft(org, &key)?;
                state.store.log_event(
                    "release-cut",
                    Some(org),
                    &format!("{key} {} by {actor} (bulk)", t.draft.version),
                )?;
                lines.push(format!("{}: released {}", t.app, t.draft.version));
            }
            Err(e) => lines.push(format!("{}: FAILED tag — {e:#}", t.app)),
        }
    }
    Ok(lines)
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
    let decl = app_decl(state, org, app).await;
    let repo = decl
        .as_ref()
        .map(|d| d.repo().to_string())
        .unwrap_or_else(|| app.to_string());
    // Try the tag verbatim first (`vX.Y.Z` / a solo app's tag), then the app's
    // *configured* per-app scoped tag (ADR 0020 — covers a scope that differs
    // from the repo name), then the legacy changesets form `@<repo>/<leaf>@<ver>`.
    // `/commits/{ref}` resolves any ref; the scoped ref needs URL-encoding.
    let mut refs = vec![tag.to_string()];
    if let Some(d) = &decl {
        if d.is_per_app_release() {
            // Try both `@<scope>/<leaf>@vX.Y.Z` and the bare form — the git tag's
            // prefix can differ from the recorded (image) version's.
            for t in scoped_tag_variants(d, tag) {
                if !refs.contains(&t) {
                    refs.push(t);
                }
            }
        }
    }
    if repo != app {
        let leaf = decl
            .as_ref()
            .map(|d| d.image_leaf().to_string())
            .unwrap_or_else(|| {
                app.strip_prefix(&format!("{repo}-"))
                    .unwrap_or(app)
                    .to_string()
            });
        let ver = tag.strip_prefix('v').unwrap_or(tag);
        let legacy = format!("@{repo}/{leaf}@{ver}");
        if !refs.contains(&legacy) {
            refs.push(legacy);
        }
    }
    let mut last_err = None;
    for r in &refs {
        let enc = r.replace('@', "%40").replace('/', "%2F");
        let res: Result<serde_json::Value, _> = client
            .get(format!("/repos/{org}/{repo}/commits/{enc}"), None::<&()>)
            .await;
        match res {
            Ok(commit) => {
                if let Some(sha) = commit["sha"].as_str() {
                    return Ok(sha.to_string());
                }
            }
            Err(e) => last_err = Some(anyhow::Error::from(e)),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("commit lookup returned no sha")))
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

/// The store versions to prune during a reconcile: version-tagged releases whose
/// tag is no longer present in the registry's set. Pure — the caller supplies the
/// guard (only reconcile when the registry set is complete + non-empty).
fn stale_versions(
    store_versions: &[String],
    registry: &std::collections::HashSet<String>,
) -> Vec<String> {
    store_versions
        .iter()
        .filter(|v| crate::digest::is_version_tag(v) && !registry.contains(v.as_str()))
        .cloned()
        .collect()
}

/// Reconcile `org/app`'s releases against the GHCR registry (ADR 0009). The
/// registry's tag→digest map is authoritative, so this enumerates every
/// container version and: **records** each version-tagged one not already known
/// (self-heal for a missed `registry_package` webhook — idempotent), and
/// **prunes** store releases whose version tag no longer exists in the registry
/// (e.g. a tag deleted upstream). Returns `(recorded, pruned)`.
///
/// Pruning is guarded: only when the listing completed (didn't hit the page cap)
/// **and** found ≥1 version tag, so an API hiccup or an empty/renamed package can
/// never wipe the store. Deploy-safe — it only edits the release *store*; the
/// stable/production git pins are untouched (production moves only via promote).
/// Needs `read:packages` on the GHCR PAT.
pub async fn backfill(state: &AppState, org: &str, app: &str) -> Result<(usize, usize)> {
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
    // Every version tag the registry currently has (for the reconcile prune).
    let mut registry_versions: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut complete = false;
    // Paginate defensively (cap at 10×100 versions) so a huge package can't spin
    // forever; a break on a short page ends it early (and proves completeness).
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
                if crate::digest::is_version_tag(tag) {
                    registry_versions.insert(tag.to_string());
                    if known.insert(tag.to_string()) {
                        let image = format!("{image_base}@{digest}");
                        record(state, org, app, tag, &image).await?;
                        recorded += 1;
                    }
                }
            }
        }
        if count < 100 {
            complete = true;
            break;
        }
    }

    // Reconcile: prune store releases whose tag vanished from the registry.
    // Guarded so a partial/empty listing can never mass-delete.
    let mut pruned = 0;
    if complete && !registry_versions.is_empty() {
        let have: Vec<String> = state
            .store
            .releases(org, app)?
            .into_iter()
            .map(|r| r.version)
            .collect();
        for v in stale_versions(&have, &registry_versions) {
            if state.store.delete_release(org, app, &v)? {
                state.store.log_event(
                    "release-pruned",
                    Some(org),
                    &format!("{app} {v} (tag gone from registry)"),
                )?;
                pruned += 1;
            }
        }
    }
    tracing::info!(org, app, recorded, pruned, "release reconcile complete");
    Ok((recorded, pruned))
}

/// `POST /api/releases/{org}/{app}/backfill` — reconcile releases with the
/// registry (ADR 0009): record missed `vX.Y.Z` publishes and prune records whose
/// tag was deleted upstream. Developer-gated (a stable-class recovery, not a
/// production change — production still moves only via promote).
pub async fn backfill_post(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    crate::authz::require(&state, &headers, &org, Role::Developer)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let (recorded, pruned) = backfill(&state, &org, &app)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(format!(
        "reconciled {app} with the registry: recorded {recorded}, pruned {pruned}"
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
        base_tag_candidates, classify_bump, generate_changelog, next_version, parse_semver,
        production_overlay, LastRelease,
    };
    use majnet_common::project::{default_bump_rules, AppDecl, Autorelease, Bump, ReleaseConfig};
    use std::collections::BTreeMap;

    fn per_app_decl(name: &str, repo: &str, scope: &str) -> AppDecl {
        AppDecl {
            name: name.into(),
            template: "byo".into(),
            repo: Some(repo.into()),
            release: Some(ReleaseConfig {
                scope: Some(scope.into()),
                autorelease: Autorelease::Off,
                paths: vec![],
                bumps: None,
            }),
        }
    }

    fn msgs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn changelog_groups_by_conventional_type() {
        let cl = generate_changelog(
            &msgs(&[
                "feat(api): add CSV export (#41)",
                "fix: null deref on empty query (#43)",
                "chore: bump deps",
                "refactor!: drop the v1 endpoint",
                "docs: tidy readme",
                "Merge branch 'main' into feature",
            ]),
            &default_bump_rules(),
        );
        // Sections present in priority order; merge commit dropped.
        let breaking = cl.find("Breaking changes").unwrap();
        let feats = cl.find("Features").unwrap();
        let fixes = cl.find("Fixes").unwrap();
        assert!(breaking < feats && feats < fixes);
        // The `type(scope):` prefix is stripped from the displayed line.
        assert!(cl.contains("- add CSV export (#41)"));
        assert!(cl.contains("- drop the v1 endpoint"));
        // Non-feat/fix/breaking commits are ignored — no "Other changes", and the
        // chore/docs lines don't appear.
        assert!(!cl.contains("Other changes"));
        assert!(!cl.contains("- bump deps"));
        assert!(!cl.contains("tidy readme"));
        assert!(!cl.contains("Merge branch"));
    }

    #[test]
    fn changelog_empty_is_placeholder() {
        assert_eq!(
            generate_changelog(&[], &default_bump_rules()),
            "_No notable changes._\n"
        );
        // A lone merge commit produces no entries either.
        assert_eq!(
            generate_changelog(&msgs(&["Merge pull request #9"]), &default_bump_rules()),
            "_No notable changes._\n"
        );
    }

    #[test]
    fn custom_bump_rules_override_the_defaults() {
        let rules = BTreeMap::from([
            ("feat".to_string(), Bump::Minor),
            ("fix".to_string(), Bump::Patch),
            ("perf".to_string(), Bump::Minor), // custom: perf counts as minor
        ]);
        assert_eq!(
            classify_bump(&msgs(&["perf: faster"]), &rules),
            Some("minor")
        );
        // a type absent from the map is still ignored
        assert_eq!(classify_bump(&msgs(&["docs: x"]), &rules), None);
        // breaking is always major regardless of the map
        assert_eq!(
            classify_bump(&msgs(&["perf!: drop cache"]), &rules),
            Some("major")
        );
        // the changelog groups a custom minor type under Features
        let cl = generate_changelog(&msgs(&["perf: faster"]), &rules);
        assert!(cl.contains("Features") && cl.contains("- faster"), "{cl}");
    }

    #[test]
    fn auto_bump_from_conventional_commits() {
        let r = default_bump_rules();
        // a `fix` → patch; the chore/docs alongside are ignored
        assert_eq!(
            classify_bump(&msgs(&["fix: a", "chore: deps", "docs: x"]), &r),
            Some("patch")
        );
        // any feat wins over a fix
        assert_eq!(
            classify_bump(&msgs(&["fix: a", "feat(api): b"]), &r),
            Some("minor")
        );
        // breaking wins over everything (any `type!`)
        assert_eq!(
            classify_bump(&msgs(&["feat: a", "refactor!: drop v1"]), &r),
            Some("major")
        );
        assert_eq!(
            classify_bump(&msgs(&["fix: a\n\nBREAKING CHANGE: db reset"]), &r),
            Some("major")
        );
        // `feat!:` header is breaking, not just a feature
        assert_eq!(classify_bump(&msgs(&["feat!: rewrite"]), &r), Some("major"));
        // nothing releasable → None: empty, only-ignored types, non-conventional
        assert_eq!(classify_bump(&[], &r), None);
        assert_eq!(classify_bump(&msgs(&["chore: deps", "docs: x"]), &r), None);
        assert_eq!(classify_bump(&msgs(&["wip", "merge branch"]), &r), None);
    }

    #[test]
    fn semver_parse_and_bump() {
        assert_eq!(parse_semver("v1.4.2"), Some((1, 4, 2)));
        assert_eq!(parse_semver("v0.0.3"), Some((0, 0, 3)));
        assert_eq!(parse_semver("v1.2.3-rc1"), Some((1, 2, 3)));
        // Bare (no `v`) versions — changesets tags releases with the raw version.
        assert_eq!(parse_semver("0.30.6"), Some((0, 30, 6)));
        assert_eq!(parse_semver("1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_semver("latest"), None);
        assert_eq!(parse_semver("v1.2"), None);
        assert_eq!(parse_semver("1.2"), None);
        assert_eq!(next_version(Some((0, 0, 3)), "patch").unwrap(), "0.0.4");
        assert_eq!(next_version(Some((0, 0, 3)), "minor").unwrap(), "0.1.0");
        assert_eq!(next_version(Some((1, 4, 2)), "major").unwrap(), "2.0.0");
        assert_eq!(next_version(None, "patch").unwrap(), "0.0.1");
        assert_eq!(next_version(None, "minor").unwrap(), "0.1.0");
        assert!(next_version(None, "huge").is_err());
    }

    #[test]
    fn last_release_preserves_prefix_and_offers_scoped_tag() {
        // A monorepo member with a bare recorded version → bare output prefix,
        // and its candidate refs include the changesets scoped git tag.
        let bare = LastRelease {
            ver: (0, 30, 6),
            app: "sideline-bot".into(),
            prefix: "",
        };
        assert_eq!(bare.display(), "0.30.6");
        let cands = bare.tag_candidates("sideline");
        assert!(cands.contains(&"0.30.6".to_string()));
        assert!(cands.contains(&"@sideline/bot@0.30.6".to_string()));

        // A solo app with a `v`-prefixed version → `v` output, no scoped tag.
        let vpref = LastRelease {
            ver: (1, 2, 3),
            app: "blog".into(),
            prefix: "v",
        };
        assert_eq!(vpref.display(), "v1.2.3");
        assert!(!vpref
            .tag_candidates("blog")
            .iter()
            .any(|c| c.starts_with('@')));
    }

    #[test]
    fn base_candidates_prepend_configured_scoped_tag() {
        // scope differs from the repo name → LastRelease::tag_candidates can't
        // produce it, so base_tag_candidates must prepend the configured tag.
        let decl = per_app_decl("sideline-server", "sideline", "acme");
        let last = LastRelease {
            ver: (0, 39, 0),
            app: "sideline-server".into(),
            prefix: "v",
        };
        let cands = base_tag_candidates(Some(&decl), "sideline", &last);
        // The `v` scoped form is first (most specific), and the bare scoped form
        // is also present — the git tag's prefix may differ from the version's.
        assert_eq!(cands[0], "@acme/server@v0.39.0", "{cands:?}");
        assert!(
            cands.iter().any(|c| c == "@acme/server@0.39.0"),
            "{cands:?}"
        );
        // Generic fallbacks remain.
        assert!(cands.iter().any(|c| c == "v0.39.0"));
    }

    #[test]
    fn cut_creates_v_prefixed_scoped_tag() {
        use super::release_tag_to_create;
        let per_app = per_app_decl("sideline-server", "sideline", "sideline");
        // A bare recorded version still cuts the `v`-prefixed scoped git tag
        // (Changesets convention), and a `v` input doesn't double up.
        assert_eq!(
            release_tag_to_create(Some(&per_app), "0.39.0"),
            "@sideline/server@v0.39.0"
        );
        assert_eq!(
            release_tag_to_create(Some(&per_app), "v0.39.0"),
            "@sideline/server@v0.39.0"
        );
        // Repo-wide / solo → the version verbatim (its own preserved prefix).
        let repo_wide = AppDecl {
            name: "blog".into(),
            template: "web-app".into(),
            repo: None,
            release: None,
        };
        assert_eq!(release_tag_to_create(Some(&repo_wide), "v1.2.3"), "v1.2.3");
        assert_eq!(release_tag_to_create(None, "0.5.0"), "0.5.0");
    }

    #[test]
    fn scoped_variants_cover_both_prefixes_from_either_input() {
        use super::scoped_tag_variants;
        let decl = per_app_decl("sideline-server", "sideline", "sideline");
        // A bare recorded version still yields the `v` git-tag spelling…
        let from_bare = scoped_tag_variants(&decl, "0.38.7");
        assert!(from_bare.contains(&"@sideline/server@v0.38.7".to_string()));
        assert!(from_bare.contains(&"@sideline/server@0.38.7".to_string()));
        // …and a `v`-prefixed input normalizes to the same pair (no `@…@vv…`).
        assert_eq!(scoped_tag_variants(&decl, "v0.38.7"), from_bare);
    }

    #[test]
    fn reconcile_prunes_only_versions_gone_from_the_registry() {
        use super::stale_versions;
        use std::collections::HashSet;
        let have = vec![
            "v0.39.0".to_string(), // deleted upstream → prune
            "0.38.7".to_string(),  // still in registry → keep
            "0.38.6".to_string(),  // still in registry → keep
        ];
        let registry: HashSet<String> = ["0.38.7", "0.38.6", "0.38.5"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            stale_versions(&have, &registry),
            vec!["v0.39.0".to_string()]
        );
        // Nothing stale when every stored version is still present.
        assert!(stale_versions(&have[1..], &registry).is_empty());
    }

    #[test]
    fn set_json_version_preserves_formatting_and_key_order() {
        use super::set_json_version;
        let pkg = "{\n  \"name\": \"@sideline/server\",\n  \"version\": \"0.38.7\",\n  \"private\": true,\n  \"dependencies\": { \"x\": \"^1.0.0\" }\n}\n";
        let out = set_json_version(pkg, "0.39.0").unwrap();
        assert!(out.contains("\"version\": \"0.39.0\""));
        // Everything else untouched (name first, deps intact, no reordering).
        assert!(out.starts_with("{\n  \"name\": \"@sideline/server\","));
        assert!(out.contains("\"dependencies\": { \"x\": \"^1.0.0\" }"));
        assert!(!out.contains("0.38.7"));
        // No version key → error, not a silent no-op.
        assert!(set_json_version("{\"name\":\"x\"}", "1.0.0").is_err());
    }

    #[test]
    fn changelog_entry_demotes_sections_under_the_version() {
        use super::changelog_entry;
        let notes = "## 🚀 Features\n- add export\n## 🐛 Fixes\n- npe";
        let e = changelog_entry("0.39.0", notes);
        assert!(e.starts_with("## 0.39.0\n\n"));
        assert!(e.contains("### 🚀 Features"));
        assert!(e.contains("### 🐛 Fixes"));
        assert!(!e.contains("\n## 🚀")); // demoted, no stray H2 sections
    }

    #[test]
    fn prepend_changelog_keeps_title_and_stacks_newest_first() {
        use super::prepend_changelog;
        // Fresh file.
        let fresh = prepend_changelog(None, "## 0.1.0\n\n- first\n");
        assert_eq!(fresh, "# Changelog\n\n## 0.1.0\n\n- first\n");
        // Existing with a title → new entry goes under the title, above the old.
        let existing = "# Changelog\n\n## 0.1.0\n\n- first\n";
        let out = prepend_changelog(Some(existing), "## 0.2.0\n\n- second\n");
        assert!(out.starts_with("# Changelog\n\n## 0.2.0"));
        assert!(out.find("0.2.0").unwrap() < out.find("0.1.0").unwrap());
    }

    #[test]
    fn app_dir_is_the_first_path_prefix_for_per_app_only() {
        use super::app_dir;
        let mut d = per_app_decl("sideline-server", "sideline", "sideline");
        d.release.as_mut().unwrap().paths =
            vec!["applications/server/**".into(), "packages/shared/**".into()];
        assert_eq!(app_dir(Some(&d)).as_deref(), Some("applications/server"));
        // No paths → None (release just tags).
        let d2 = per_app_decl("x", "r", "r");
        assert_eq!(app_dir(Some(&d2)), None);
        // Repo-wide (no scope) → None even with paths.
        let repo_wide = AppDecl {
            name: "blog".into(),
            template: "web-app".into(),
            repo: None,
            release: None,
        };
        assert_eq!(app_dir(Some(&repo_wide)), None);
    }

    #[test]
    fn glob_to_prefix_takes_the_literal_directory() {
        use super::glob_to_prefix;
        assert_eq!(
            glob_to_prefix("applications/server/**"),
            "applications/server"
        );
        assert_eq!(glob_to_prefix("packages/shared/**"), "packages/shared");
        assert_eq!(glob_to_prefix("applications/web/*.ts"), "applications/web");
        // No glob → the path itself (trailing slash trimmed).
        assert_eq!(glob_to_prefix("apps/api/"), "apps/api");
        // Leading glob → empty (not path-filterable → whole-repo fallback).
        assert_eq!(glob_to_prefix("**/Dockerfile"), "");
    }

    #[test]
    fn autorelease_path_globs_match_the_right_app() {
        use super::paths_match;
        let pats = vec![
            "applications/server/**".to_string(),
            "packages/shared/**".to_string(),
        ];
        assert!(paths_match(
            &pats,
            &["applications/server/src/index.ts".into()]
        ));
        assert!(paths_match(&pats, &["packages/shared/util.ts".into()]));
        // A sibling app's change doesn't match.
        assert!(!paths_match(&pats, &["applications/web/index.ts".into()]));
        // `*` doesn't cross `/`: a shallow glob won't match a nested file.
        assert!(!paths_match(
            &["applications/server/*".to_string()],
            &["applications/server/src/index.ts".into()]
        ));
        // Nothing changed / no patterns ⇒ no match.
        assert!(!paths_match(&pats, &[]));
        assert!(!paths_match(&[], &["applications/server/x".into()]));
    }

    #[test]
    fn base_candidates_repo_wide_stay_generic() {
        // A repo-wide app (no release block) yields only the generic refs.
        let decl = AppDecl {
            name: "blog".into(),
            template: "web-app".into(),
            repo: None,
            release: None,
        };
        let last = LastRelease {
            ver: (1, 2, 3),
            app: "blog".into(),
            prefix: "v",
        };
        let cands = base_tag_candidates(Some(&decl), "blog", &last);
        assert!(cands.contains(&"v1.2.3".to_string()));
        assert!(!cands.iter().any(|c| c.starts_with('@')));
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
