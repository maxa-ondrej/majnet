//! Dashboard write API (§16, phase 5): manifest editing + member management.
//! Every write is a bot-authored commit on ops `main` — through git, never
//! around it; the render pipeline propagates from there. Role-gated via
//! `authz` (production overlay + members = project admin, rest = developer).

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use majnet_common::manifest::AppManifest;
use majnet_common::merge::merge;
use majnet_common::platform::{Node, NodesFile, ProjectRegistryEntry, ProjectsFile};
use majnet_common::project::{AppDecl, Member, ProjectConfig, Role};
use majnet_common::EnvClass;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::AppState;

pub(crate) type ApiError = (StatusCode, String);

fn bad_gateway(e: anyhow::Error) -> ApiError {
    (StatusCode::BAD_GATEWAY, format!("{e:#}"))
}
fn bad_request(msg: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, msg.into())
}

const MANIFEST_FILES: [&str; 5] = [
    "base.yaml",
    "testing.yaml",
    "stable.yaml",
    "production.yaml",
    "ephemeral.yaml",
];

/// One manifest file, as raw YAML plus its parsed structure — the raw form
/// feeds the editor's escape hatch, the parsed form feeds the field builder.
#[derive(Serialize)]
pub struct ManifestFile {
    pub yaml: String,
    /// Parsed structure (sparse overlays allowed); `null` if the file is empty.
    pub data: serde_json::Value,
}

/// `GET /api/manifest/{org}/{app}` — the app's manifest files on ops `main`,
/// each as raw YAML + parsed data.
pub async fn manifest_get(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
) -> Result<Json<BTreeMap<String, ManifestFile>>, ApiError> {
    check_name(&app)?;
    let files = app_files(&state, &org, &app).await.map_err(bad_gateway)?;
    let out = files
        .into_iter()
        .map(|(name, yaml)| {
            let data = serde_yaml::from_str(&yaml).unwrap_or(serde_json::Value::Null);
            (name, ManifestFile { yaml, data })
        })
        .collect();
    Ok(Json(out))
}

/// `PUT /api/manifest/{org}/{app}/{file}` — validate + commit one manifest
/// file. Body is raw YAML, or JSON (serialized to YAML server-side) when the
/// content-type is `application/json` — so the form builder sends structure
/// and the raw editor sends text, both through the same validation.
pub async fn manifest_put(
    State(state): State<Arc<AppState>>,
    Path((org, app, file)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: String,
) -> Result<String, ApiError> {
    check_name(&app)?;
    if !MANIFEST_FILES.contains(&file.as_str()) {
        return Err(bad_request(format!(
            "file must be one of {MANIFEST_FILES:?}"
        )));
    }
    // Form builder posts JSON; convert to YAML so the rest of the path (and the
    // committed file) is identical to a raw-YAML edit.
    let is_json = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.contains("application/json"));
    let body = if is_json {
        let value: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| bad_request(format!("invalid JSON: {e}")))?;
        serde_yaml::to_string(&value).map_err(|e| bad_gateway(e.into()))?
    } else {
        body
    };
    // The production overlay is a production action (§9: role admin).
    let min_role = if file == "production.yaml" {
        Role::Admin
    } else {
        Role::Developer
    };
    let actor = crate::authz::require(&state, &headers, &org, min_role)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;

    let mut files = app_files(&state, &org, &app).await.map_err(bad_gateway)?;
    files.insert(file.clone(), body.clone());
    validate_app_files(&app, &files).map_err(|e| bad_request(format!("{e:#}")))?;

    let path = format!("apps/{app}/{file}");
    let message = format!("manifest({app}): edit {file} via dashboard by {actor}");
    commit_file(&state, &org, &path, &body, &message)
        .await
        .map_err(bad_gateway)?;
    state
        .store
        .log_event("manifest-edit", Some(&org), &format!("{path} by {actor}"))
        .map_err(bad_gateway)?;
    Ok(format!(
        "{path} committed; render PRs will propagate the change"
    ))
}

/// `GET /api/members/{org}` — project.yaml members.
pub async fn members_get(
    State(state): State<Arc<AppState>>,
    Path(org): Path<String>,
) -> Result<Json<Vec<Member>>, ApiError> {
    let project = read_project(&state, &org).await.map_err(bad_gateway)?;
    Ok(Json(project.members))
}

#[derive(Deserialize)]
pub struct MemberChange {
    pub user: String,
    /// `admin` | `developer` | `remove`.
    pub role: String,
}

/// `POST /api/members/{org}` — upsert or remove one member (admin-only).
/// The bot's org sync propagates teams + Tailscale ACLs from the commit.
pub async fn members_post(
    State(state): State<Arc<AppState>>,
    Path(org): Path<String>,
    headers: HeaderMap,
    Json(change): Json<MemberChange>,
) -> Result<String, ApiError> {
    let actor = crate::authz::require(&state, &headers, &org, Role::Admin)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    if change.user.is_empty()
        || !change
            .user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(bad_request("invalid GitHub username"));
    }

    let mut project = read_project(&state, &org).await.map_err(bad_gateway)?;
    let action = match change.role.as_str() {
        "remove" => {
            let before = project.members.len();
            project.members.retain(|m| m.user != change.user);
            if project.members.len() == before {
                return Err(bad_request(format!("{} is not a member", change.user)));
            }
            format!("remove {}", change.user)
        }
        role @ ("admin" | "developer") => {
            let parsed: Role = serde_yaml::from_str(role).expect("checked");
            match project.members.iter_mut().find(|m| m.user == change.user) {
                Some(member) => member.role = parsed,
                None => project.members.push(Member {
                    user: change.user.clone(),
                    role: parsed,
                }),
            }
            format!("{} → {role}", change.user)
        }
        other => {
            return Err(bad_request(format!(
                "role must be admin|developer|remove, got {other}"
            )))
        }
    };

    let yaml = serde_yaml::to_string(&project).map_err(|e| bad_gateway(e.into()))?;
    let message = format!("members: {action} via dashboard by {actor}");
    commit_file(&state, &org, "project.yaml", &yaml, &message)
        .await
        .map_err(bad_gateway)?;
    state
        .store
        .log_event("member-change", Some(&org), &format!("{action} by {actor}"))
        .map_err(bad_gateway)?;
    Ok(format!("{action} committed; org sync will propagate"))
}

// ── helpers ────────────────────────────────────────────────────────────────

fn check_name(app: &str) -> Result<(), ApiError> {
    if app.is_empty()
        || !app
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(bad_request("invalid app name"));
    }
    Ok(())
}

/// The app's manifest files from the ops `main` snapshot.
pub(crate) async fn app_files(
    state: &AppState,
    org: &str,
    app: &str,
) -> Result<BTreeMap<String, String>> {
    let (_, tar) = crate::proxy::fetch_snapshot(state, org, "ops", "main").await?;
    let sources = majnet_common::tarball::untar(&tar)?;
    let prefix = format!("apps/{app}/");
    let mut files = BTreeMap::new();
    for (path, bytes) in sources {
        if let Some(name) = path.strip_prefix(&prefix) {
            if MANIFEST_FILES.contains(&name) {
                files.insert(name.to_string(), String::from_utf8(bytes)?);
            }
        }
    }
    Ok(files)
}

/// Validate the app's files as the render pipeline would see them after the
/// change: every present overlay must merge with base into a valid manifest.
pub(crate) fn validate_app_files(app: &str, files: &BTreeMap<String, String>) -> Result<()> {
    let base_str = files
        .get("base.yaml")
        .context("the app has no base.yaml — create it first")?;
    let base: serde_yaml::Value = serde_yaml::from_str(base_str).context("base.yaml")?;
    let overlays: Vec<&str> = files
        .keys()
        .map(String::as_str)
        .filter(|f| *f != "base.yaml")
        .collect();
    anyhow::ensure!(
        !overlays.is_empty(),
        "no class overlay present — the app would not render into any class"
    );
    for overlay_file in overlays {
        let overlay: serde_yaml::Value =
            serde_yaml::from_str(&files[overlay_file]).with_context(|| overlay_file.to_string())?;
        let mut merged = merge(base.clone(), overlay);
        // Same name handling as render.rs: directory is the identity.
        if let serde_yaml::Value::Mapping(map) = &mut merged {
            let key = serde_yaml::Value::from("name");
            if map.get(&key).is_none() {
                map.insert(key, serde_yaml::Value::from(app));
            }
        }
        let yaml = serde_yaml::to_string(&merged)?;
        AppManifest::parse(&yaml)
            .with_context(|| format!("base.yaml ⊕ {overlay_file} is not a valid manifest"))?;
    }
    Ok(())
}

/// Create-or-update one file on ops `main`.
pub(crate) async fn commit_file(
    state: &AppState,
    org: &str,
    path: &str,
    content: &str,
    message: &str,
) -> Result<()> {
    let client = state.github.org_client(org).await?;
    let repos = client.repos(org, "ops");
    match crate::promote::read_file(&repos, path).await? {
        Some((current, sha)) => {
            if current == content {
                return Ok(());
            }
            repos
                .update_file(path, message, content, &sha)
                .branch("main")
                .send()
                .await?;
        }
        None => {
            repos
                .create_file(path, message, content)
                .branch("main")
                .send()
                .await?;
        }
    }
    Ok(())
}

pub(crate) async fn read_project(state: &AppState, org: &str) -> Result<ProjectConfig> {
    let (_, tar) = crate::proxy::fetch_snapshot(state, org, "ops", "main").await?;
    let files = majnet_common::tarball::untar(&tar)?;
    let yaml = files
        .get("project.yaml")
        .with_context(|| format!("{org}/ops has no project.yaml"))?;
    serde_yaml::from_slice(yaml).context("parsing project.yaml")
}

// ── whoami ───────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct WhoAmI {
    /// GitHub login of the acting user, or `null` for a header-less infra call.
    pub login: Option<String>,
    /// Whether the actor is a platform admin (`people.yaml`).
    pub admin: bool,
}

/// `GET /api/whoami` — the acting identity, for the dashboard sidebar + gating.
/// Infallible: an unmapped login (or a transient snapshot hiccup) reports the
/// raw identity as unprivileged rather than 502-ing the whole shell — the
/// gated endpoints still enforce real authorization.
pub async fn whoami(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Json<WhoAmI> {
    use majnet_common::authz::Actor;
    match crate::authz::actor(&state, &headers).await {
        Ok(Actor::Infra) => Json(WhoAmI {
            login: None,
            admin: true,
        }),
        Ok(Actor::Human {
            github,
            platform_admin,
        }) => Json(WhoAmI {
            login: Some(github),
            admin: platform_admin,
        }),
        Err(e) => {
            let login = headers
                .get("tailscale-user-login")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            tracing::warn!(
                error = format!("{e:#}"),
                ?login,
                "whoami: unresolved identity"
            );
            Json(WhoAmI {
                login,
                admin: false,
            })
        }
    }
}

// ── projects ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ProjectSummary {
    pub name: String,
    pub org: String,
    /// The App is installed and the `ops` repo is reachable.
    pub onboarded: bool,
    /// Number of apps declared under `apps/*/base.yaml` on ops `main`.
    pub apps: usize,
}

/// `GET /api/projects` — the project registry (`projects.yaml`) enriched with
/// onboarding state. Discovery needs both the registry entry and the installed
/// App (§2); an entry whose `ops` repo we can't reach shows as not-onboarded.
pub async fn projects_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<ProjectSummary>>, ApiError> {
    let registry = read_projects(&state).await.map_err(bad_gateway)?;
    let mut out = Vec::with_capacity(registry.projects.len());
    for entry in registry.projects {
        // Best-effort: an uninstalled/absent ops repo is "pending", not an error.
        let apps = match app_names(&state, &entry.org).await {
            Ok(names) => Some(names.len()),
            Err(_) => None,
        };
        out.push(ProjectSummary {
            name: entry.name,
            org: entry.org,
            onboarded: apps.is_some(),
            apps: apps.unwrap_or(0),
        });
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct NewProject {
    pub name: String,
    pub org: String,
}

/// `POST /api/projects` — register a project in `projects.yaml` (platform
/// admin). Org creation stays on GitHub; this just gates discovery. The
/// hourly/webhook org sync materializes the `ops` repo from there.
pub async fn projects_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<NewProject>,
) -> Result<String, ApiError> {
    let actor = crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let is_slug = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    };
    if !is_slug(&req.name) {
        return Err(bad_request("invalid project name (lowercase, digits, -)"));
    }
    if !is_slug(&req.org) {
        return Err(bad_request("invalid GitHub org (lowercase, digits, -)"));
    }

    let mut registry = read_projects(&state).await.map_err(bad_gateway)?;
    if registry.projects.iter().any(|p| p.org == req.org) {
        return Err(bad_request(format!("{} is already registered", req.org)));
    }
    registry.projects.push(ProjectRegistryEntry {
        name: req.name.clone(),
        org: req.org.clone(),
    });
    let yaml = format!(
        "# Managed by the platform — project registry (§2).\n{}",
        serde_yaml::to_string(&registry).map_err(|e| bad_gateway(e.into()))?
    );
    let message = format!(
        "projects: register {} ({}) via dashboard by {actor}",
        req.name, req.org
    );
    commit_platform_file(&state, "projects.yaml", &yaml, &message)
        .await
        .map_err(bad_gateway)?;
    state
        .store
        .log_event(
            "project-registered",
            Some(&req.org),
            &format!("{} by {actor}", req.name),
        )
        .map_err(bad_gateway)?;
    Ok(format!(
        "{} registered; install the App at https://github.com/apps/majnet-platform/installations/new and the ops repo will be created on the next sync",
        req.org
    ))
}

// ── apps ─────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct AppSummary {
    pub name: String,
    pub image: String,
    /// Class overlays present under `apps/<name>/` (stable/production/ephemeral).
    pub classes: Vec<String>,
    /// Primary ingress host, if the manifest declares ingress.
    pub host: Option<String>,
    /// All ingress hosts (primary + additional domains).
    pub domains: Vec<String>,
    /// Managed database engine, if any.
    pub database: Option<String>,
}

/// `GET /api/apps/{org}` — one summary per app declared on the project's ops
/// `main` (`apps/<name>/base.yaml` ⊕ overlays).
pub async fn apps_get(
    State(state): State<Arc<AppState>>,
    Path(org): Path<String>,
) -> Result<Json<Vec<AppSummary>>, ApiError> {
    let (_, tar) = crate::proxy::fetch_snapshot(&state, &org, "ops", "main")
        .await
        .map_err(bad_gateway)?;
    let files = majnet_common::tarball::untar(&tar).map_err(bad_gateway)?;
    let mut names: Vec<String> = files
        .keys()
        .filter_map(|p| p.strip_prefix("apps/"))
        .filter_map(|rest| rest.strip_suffix("/base.yaml"))
        .map(str::to_string)
        .collect();
    names.sort();

    let text: BTreeMap<String, String> = files
        .into_iter()
        .filter_map(|(p, b)| String::from_utf8(b).ok().map(|s| (p, s)))
        .collect();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        match summarize_app(&name, &text) {
            Ok(summary) => out.push(summary),
            Err(e) => tracing::warn!(
                org,
                name,
                error = format!("{e:#}"),
                "skipping unparsable app"
            ),
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize, Serialize, Clone)]
pub struct NewApp {
    pub name: String,
    /// Digest-pinned image. Optional — when omitted, a placeholder at the app's
    /// eventual GHCR path is used until CI builds a real one (production still
    /// moves via promote).
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub domains: Vec<String>,
    /// Class overlays to create — at least one of stable/production/ephemeral.
    pub classes: Vec<String>,
    /// `postgres` | `mariadb` | `valkey` | `mongodb` | none.
    #[serde(default)]
    pub database: Option<String>,
    /// Starter template for the app's source repo (`repo-templates/<template>/`
    /// in the platform repo). Ignored when `create_repo` is false. The app is
    /// declared in `project.yaml`, and org-sync materializes the source repo
    /// from this template.
    #[serde(default)]
    pub template: String,
    /// Create a MajNet source repo (from `template`, with CI) and declare the
    /// app in `project.yaml`. When false, the app is manifests-only — it runs a
    /// prebuilt/external image, so `image` is required and no repo/CI is made.
    /// Defaults to true (and is implied by `import`).
    #[serde(default = "default_true")]
    pub create_repo: bool,
    /// Migrate an existing app instead of scaffolding fresh (ADR 0010): seed the
    /// source repo from an old GitHub repo + inject MajNet CI. `template` still
    /// selects which CI workflows to inject.
    #[serde(default)]
    pub import: Option<crate::migrate::ImportSource>,
}

fn default_true() -> bool {
    true
}

/// `POST /api/apps/{org}` — scaffold a new app's `base.yaml` + selected class
/// overlays on ops `main`. Creating a production overlay is an admin action.
pub async fn apps_post(
    State(state): State<Arc<AppState>>,
    Path(org): Path<String>,
    headers: HeaderMap,
    Json(mut req): Json<NewApp>,
) -> Result<String, ApiError> {
    check_name(&req.name)?;
    let valid_classes = ["testing", "stable", "production", "ephemeral"];
    if req.classes.is_empty()
        || !req
            .classes
            .iter()
            .all(|c| valid_classes.contains(&c.as_str()))
    {
        return Err(bad_request(
            "classes must be a non-empty subset of stable|production|ephemeral",
        ));
    }
    let wants_production = req.classes.iter().any(|c| c == "production");
    let min_role = if wants_production {
        Role::Admin
    } else {
        Role::Developer
    };
    let actor = crate::authz::require(&state, &headers, &org, min_role)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;

    // Refuse to clobber an existing app — unless a failed import is on record
    // for it, in which case re-submitting resumes/overwrites that import.
    let mut files = app_files(&state, &org, &req.name)
        .await
        .map_err(bad_gateway)?;
    if !files.is_empty() {
        let retrying = state
            .store
            .imports(&org)
            .map_err(bad_gateway)?
            .iter()
            .any(|i| i.app == req.name && i.status == "failed");
        if !retrying {
            return Err(bad_request(format!("app {} already exists", req.name)));
        }
    }

    // A source repo is made when scaffolding from a template or importing.
    let wants_repo = req.create_repo || req.import.is_some();

    if wants_repo {
        // The source repo is scaffolded from this template by org-sync; validate
        // it exists now so a typo is rejected here rather than failing later in
        // a background sync.
        if req.template.trim().is_empty() {
            return Err(bad_request(
                "a source-repo template is required (e.g. web-app)",
            ));
        }
        let platform = read_platform(&state).await.map_err(bad_gateway)?;
        let tprefix = format!("repo-templates/{}/", req.template);
        if !platform.keys().any(|p| p.starts_with(&tprefix)) {
            return Err(bad_request(format!(
                "unknown template '{}' (no repo-templates/{}/ in the platform repo)",
                req.template, req.template
            )));
        }
    }

    // Image is optional only when a repo/CI (or promote) will supply one; a
    // manifests-only app (no repo) must bring a prebuilt image.
    if req.image.trim().is_empty() {
        if wants_repo {
            req.image = format!("ghcr.io/{org}/{}@sha256:{}", req.name, "0".repeat(64));
        } else {
            return Err(bad_request(
                "an image is required when not creating a source repo",
            ));
        }
    }

    let base = scaffold_base(&req).map_err(|e| bad_request(format!("{e:#}")))?;
    files.insert("base.yaml".to_string(), base);
    for class in &req.classes {
        files.insert(format!("{class}.yaml"), "{}\n".to_string());
    }
    validate_app_files(&req.name, &files).map_err(|e| bad_request(format!("{e:#}")))?;

    // Import mode (ADR 0010): seed the source repo from an old repo + inject CI,
    // then scaffold. Slow (GitHub source-import is async) → run in the
    // background and surface progress via the events feed.
    if let Some(source) = req.import.clone() {
        let app = req.name.clone();
        let repo = source.repo.clone();
        let st = state.clone();
        let org2 = org.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::migrate::import_app(&st, &org2, &req, &actor, &source).await {
                tracing::error!(
                    org = org2,
                    app = req.name,
                    error = format!("{e:#}"),
                    "app import failed"
                );
                let _ = st.store.fail_import(&org2, &req.name, &format!("{e:#}"));
                let _ = st.store.log_event(
                    "app-import-failed",
                    Some(&org2),
                    &format!("{}: {e:#}", req.name),
                );
            }
        });
        return Ok(format!(
            "importing {app} from {repo} — watch notifications; the app and its source repo appear once the import completes"
        ));
    }

    scaffold_and_declare(&state, &org, &req, &actor, req.create_repo)
        .await
        .map_err(bad_gateway)?;
    Ok(if req.create_repo {
        format!(
            "{} scaffolded ({}); source repo from template {} + render PRs will propagate",
            req.name,
            req.classes.join(", "),
            req.template
        )
    } else {
        format!(
            "{} scaffolded ({}) from image {}; no source repo — render PRs will propagate",
            req.name,
            req.classes.join(", "),
            req.image
        )
    })
}

/// Declare the app in `project.yaml` (the canonical app list) **first**, then
/// commit `base.yaml` + the selected class overlays. Shared by direct creation
/// and the import background task.
///
/// The declaration must precede the manifest commits: each ops-main commit
/// triggers an org-sync, and org-sync archives repos not yet in `project.yaml`.
/// If the manifests were committed first, that intervening sync would archive
/// the (existing, imported) repo before it's declared. For imports the repo
/// already exists, so org-sync sees it declared + present and leaves it alone.
pub(crate) async fn scaffold_and_declare(
    state: &AppState,
    org: &str,
    req: &NewApp,
    actor: &str,
    declare: bool,
) -> Result<()> {
    // A manifests-only app (no source repo) isn't declared in project.yaml, so
    // org-sync neither creates nor archives a repo for it — it just renders +
    // deploys from the provided image (like the external-image demo apps).
    if declare {
        let mut project = read_project(state, org).await?;
        if !project.apps.iter().any(|a| a.name == req.name) {
            project.apps.push(AppDecl {
                name: req.name.clone(),
                template: req.template.clone(),
            });
            let yaml = serde_yaml::to_string(&project)?;
            commit_file(
                state,
                org,
                "project.yaml",
                &yaml,
                &format!(
                    "project({}): declare app (template {}) via dashboard by {actor}",
                    req.name, req.template
                ),
            )
            .await?;
        }
    }
    let base = scaffold_base(req)?;
    commit_file(
        state,
        org,
        &format!("apps/{}/base.yaml", req.name),
        &base,
        &format!("manifest({}): scaffold via dashboard by {actor}", req.name),
    )
    .await?;
    for class in &req.classes {
        commit_file(
            state,
            org,
            &format!("apps/{}/{class}.yaml", req.name),
            "{}\n",
            &format!(
                "manifest({}): add {class} overlay via dashboard by {actor}",
                req.name
            ),
        )
        .await?;
    }
    state.store.log_event(
        "app-scaffolded",
        Some(org),
        &format!("{} [{}] by {actor}", req.name, req.classes.join(",")),
    )?;
    Ok(())
}

/// `GET /api/imports/{org}` — in-progress + failed app imports (ADR 0010), for
/// the dashboard's "importing…" skeletons + step progress.
pub async fn imports_get(
    State(state): State<Arc<AppState>>,
    Path(org): Path<String>,
) -> Result<Json<Vec<crate::state::ImportStatus>>, ApiError> {
    state.store.imports(&org).map(Json).map_err(bad_gateway)
}

/// `POST /api/imports/{org}/{app}/retry` — re-run a failed import from its
/// stored request. Tokens + env secrets are not persisted, so a private source
/// or env-secret import must be re-run from the form for those parts.
pub async fn imports_retry(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    let req_json = state
        .store
        .import_request(&org, &app)
        .map_err(bad_gateway)?
        .filter(|s| s != "{}")
        .ok_or_else(|| bad_request(format!("no import on record to retry for {app}")))?;
    let req: NewApp = serde_json::from_str(&req_json)
        .map_err(|e| bad_request(format!("stored import request is invalid: {e}")))?;
    let Some(source) = req.import.clone() else {
        return Err(bad_request("the stored request is not an import"));
    };
    let min_role = if req.classes.iter().any(|c| c == "production") {
        Role::Admin
    } else {
        Role::Developer
    };
    let actor = crate::authz::require(&state, &headers, &org, min_role)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;

    let st = state.clone();
    let org2 = org.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::migrate::import_app(&st, &org2, &req, &actor, &source).await {
            tracing::error!(
                org = org2,
                app = req.name,
                error = format!("{e:#}"),
                "app import retry failed"
            );
            let _ = st.store.fail_import(&org2, &req.name, &format!("{e:#}"));
            let _ = st.store.log_event(
                "app-import-failed",
                Some(&org2),
                &format!("{}: {e:#}", req.name),
            );
        }
    });
    Ok(format!("retrying import of {app} — watch its progress"))
}

// ── container registry (GHCR pull token, ADR 0012) ─────────────────────────────

#[derive(Serialize)]
pub struct RegistryStatus {
    /// Whether a GHCR pull token is configured (via Settings or the env
    /// bootstrap). Never reveals the token itself.
    pub configured: bool,
}

#[derive(Deserialize)]
pub struct GhcrTokenReq {
    pub token: String,
}

/// `GET /api/platform/registry` — whether a GHCR pull token is set.
pub async fn registry_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<RegistryStatus>, ApiError> {
    let configured = state
        .store
        .get_config("ghcr_token")
        .map_err(bad_gateway)?
        .is_some()
        || state.config.ghcr_token.is_some();
    Ok(Json(RegistryStatus { configured }))
}

/// `POST /api/platform/registry` — set the GHCR pull token (platform admin). A
/// classic PAT with `read:packages`; lets nodes pull private app images.
pub async fn registry_set(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<GhcrTokenReq>,
) -> Result<String, ApiError> {
    let actor = crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let token = req.token.trim();
    if token.is_empty() {
        return Err(bad_request("token is empty"));
    }
    state
        .store
        .set_config("ghcr_token", token)
        .map_err(bad_gateway)?;
    state
        .store
        .log_event("registry-token-set", None, &format!("by {actor}"))
        .map_err(bad_gateway)?;
    Ok("GHCR pull token saved — private app images can pull now".to_string())
}

// ── nodes ────────────────────────────────────────────────────────────────────

/// `GET /api/nodes` — the platform's node registry (`nodes.yaml`).
pub async fn nodes_get(State(state): State<Arc<AppState>>) -> Result<Json<Vec<Node>>, ApiError> {
    let files = read_platform(&state).await.map_err(bad_gateway)?;
    let yaml = files
        .get("nodes.yaml")
        .ok_or_else(|| bad_gateway(anyhow::anyhow!("platform repo has no nodes.yaml")))?;
    let nodes = NodesFile::parse(yaml.as_bytes()).map_err(bad_gateway)?;
    Ok(Json(nodes.nodes))
}

// ── helpers (projects/apps/nodes) ─────────────────────────────────────────────

/// Untarred platform-repo `main` snapshot, path → UTF-8 text.
pub(crate) async fn read_platform(state: &AppState) -> Result<BTreeMap<String, String>> {
    let (_, tar) =
        crate::proxy::fetch_snapshot(state, &state.config.root_org, "platform", "main").await?;
    let files = majnet_common::tarball::untar(&tar)?;
    Ok(files
        .into_iter()
        .filter_map(|(p, b)| String::from_utf8(b).ok().map(|s| (p, s)))
        .collect())
}

/// Resolve `(project_name, base_domain)` for an org from the platform repo
/// (ADR 0013) in one snapshot fetch. The base domain drives auto-assigned
/// non-production ingress hosts; it defaults to `majksa.net` when `nodes.yaml`
/// predates ADR 0013.
pub(crate) async fn project_and_domain(state: &AppState, org: &str) -> Result<(String, String)> {
    let files = read_platform(state).await?;
    let base_domain = match files.get("nodes.yaml") {
        Some(yaml) => NodesFile::parse(yaml.as_bytes())?.base_domain,
        None => default_base_domain(),
    };
    let projects = files
        .get("projects.yaml")
        .context("platform repo has no projects.yaml")?;
    let project = ProjectsFile::parse(projects.as_bytes())?
        .projects
        .into_iter()
        .find(|p| p.org == org)
        .map(|p| p.name)
        .with_context(|| format!("org '{org}' not in registry"))?;
    Ok((project, base_domain))
}

fn default_base_domain() -> String {
    "majksa.net".to_string()
}

async fn read_projects(state: &AppState) -> Result<ProjectsFile> {
    let files = read_platform(state).await?;
    let yaml = files
        .get("projects.yaml")
        .context("platform repo has no projects.yaml")?;
    ProjectsFile::parse(yaml.as_bytes()).context("parsing projects.yaml")
}

/// App directory names on the project's ops `main`.
async fn app_names(state: &AppState, org: &str) -> Result<Vec<String>> {
    let (_, tar) = crate::proxy::fetch_snapshot(state, org, "ops", "main").await?;
    let files = majnet_common::tarball::untar(&tar)?;
    Ok(files
        .keys()
        .filter_map(|p| p.strip_prefix("apps/"))
        .filter_map(|rest| rest.strip_suffix("/base.yaml"))
        .map(str::to_string)
        .collect())
}

/// Create-or-update one file on the root platform repo `main`.
async fn commit_platform_file(
    state: &AppState,
    path: &str,
    content: &str,
    message: &str,
) -> Result<()> {
    let org = &state.config.root_org;
    let client = state.github.org_client(org).await?;
    let repos = client.repos(org, "platform");
    match crate::promote::read_file(&repos, path).await? {
        Some((current, sha)) => {
            if current == content {
                return Ok(());
            }
            repos
                .update_file(path, message, content, &sha)
                .branch("main")
                .send()
                .await?;
        }
        None => {
            repos
                .create_file(path, message, content)
                .branch("main")
                .send()
                .await?;
        }
    }
    Ok(())
}

/// Merge base ⊕ (representative overlay) into a manifest summary. Picks the
/// first present overlay just to render valid YAML; the fields we surface
/// (image/ingress/database) live in base, so the choice doesn't matter.
fn summarize_app(name: &str, text: &BTreeMap<String, String>) -> Result<AppSummary> {
    let prefix = format!("apps/{name}/");
    let base_str = text
        .get(&format!("{prefix}base.yaml"))
        .context("no base.yaml")?;
    let base: serde_yaml::Value = serde_yaml::from_str(base_str).context("base.yaml")?;
    let mut classes: Vec<String> = ["testing", "stable", "production", "ephemeral"]
        .into_iter()
        .filter(|c| text.contains_key(&format!("{prefix}{c}.yaml")))
        .map(str::to_string)
        .collect();
    classes.sort();

    let overlay: serde_yaml::Value = match classes.first() {
        Some(c) => {
            serde_yaml::from_str(text.get(&format!("{prefix}{c}.yaml")).context("overlay")?)?
        }
        None => serde_yaml::Value::Mapping(Default::default()),
    };
    let mut merged = merge(base, overlay);
    if let serde_yaml::Value::Mapping(map) = &mut merged {
        let key = serde_yaml::Value::from("name");
        if map.get(&key).is_none() {
            map.insert(key, serde_yaml::Value::from(name));
        }
    }
    let manifest = AppManifest::parse(&serde_yaml::to_string(&merged)?)?;
    Ok(AppSummary {
        name: name.to_string(),
        image: manifest.image,
        classes,
        host: manifest.ingress.as_ref().and_then(|i| i.host.clone()),
        domains: manifest
            .ingress
            .as_ref()
            .map(|i| i.hosts().into_iter().map(str::to_string).collect())
            .unwrap_or_default(),
        database: manifest.database.map(|d| {
            serde_yaml::to_string(&d.engine)
                .unwrap_or_default()
                .trim()
                .to_string()
        }),
    })
}

/// Build a minimal, valid `base.yaml` from the new-app form.
fn scaffold_base(req: &NewApp) -> Result<String> {
    let mut yaml = format!("name: {}\nimage: {}\n", req.name, req.image);
    if !req.host.is_empty() {
        anyhow::ensure!(req.port != 0, "a container port is required with a domain");
    }
    // A port opts the app into routing. Non-production classes get an
    // auto-assigned host at render (ADR 0013); a `host`/`domains` here is a
    // production custom domain. So port-only is valid; a host requires a port.
    if req.port != 0 {
        yaml.push_str("ingress:\n");
        if !req.host.is_empty() {
            yaml.push_str(&format!("  host: {}\n", req.host));
        }
        yaml.push_str(&format!("  port: {}\n", req.port));
        let extra: Vec<&String> = req.domains.iter().filter(|d| !d.is_empty()).collect();
        if !req.host.is_empty() && !extra.is_empty() {
            yaml.push_str("  domains:\n");
            for d in extra {
                yaml.push_str(&format!("    - {d}\n"));
            }
        }
        yaml.push_str(&format!("health:\n  path: /\n  port: {}\n", req.port));
    }
    if let Some(engine) = req
        .database
        .as_deref()
        .filter(|e| !e.is_empty() && *e != "none")
    {
        yaml.push_str(&format!("database:\n  engine: {engine}\n"));
    }
    // Validate the base in isolation the way an overlay-merged manifest would be.
    AppManifest::parse(&yaml).context("scaffolded base.yaml is invalid")?;
    Ok(yaml)
}

/// `POST /api/secrets/{org}/{app}` — set an app's secret values for a class.
/// The values are SOPS-encrypted (to the ops `.sops.yaml` recipients) and
/// committed as `apps/{app}/secrets.{class}.yaml`, and the keys are declared in
/// the class overlay. Production is admin-gated (a render PR then gates deploy);
/// other classes are developer-gated. This replaces the class's secret set —
/// the bot can't read existing encrypted values to merge (§14).
#[derive(Deserialize)]
pub struct SetSecrets {
    pub class: String,
    /// dotenv blob: `KEY=VALUE`, one per line.
    pub env: String,
}

pub async fn secrets_post(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<SetSecrets>,
) -> Result<String, ApiError> {
    check_name(&app)?;
    let valid_classes = ["testing", "stable", "production", "ephemeral"];
    if !valid_classes.contains(&req.class.as_str()) {
        return Err(bad_request(
            "class must be one of testing|stable|production|ephemeral",
        ));
    }
    let min_role = if req.class == "production" {
        Role::Admin
    } else {
        Role::Developer
    };
    crate::authz::require(&state, &headers, &org, min_role)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;

    let n = crate::migrate::set_app_secrets(&state, &org, &app, &req.class, &req.env)
        .await
        .map_err(bad_gateway)?;
    if n == 0 {
        return Err(bad_request("no valid secrets provided"));
    }
    Ok(format!(
        "set {n} secret value(s) for {app} ({}); {}",
        req.class,
        if req.class == "production" {
            "review the render PR to deploy"
        } else {
            "auto-deploys on render"
        }
    ))
}

// ── rename ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RenameReq {
    pub new: String,
}

/// `POST /api/apps/{org}/{app}/rename` — rename an app in place (admin). Renames
/// the GitHub source repo (if any), moves `apps/<old>/*` → `apps/<new>/*` and
/// rewrites `project.yaml` in one ops-`main` commit, then renders + merges the
/// production render PR(s). Stateful apps (persistent volume or managed DB) are
/// refused here — their data-preserving rename is a separate step (M2).
pub async fn app_rename_post(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
    headers: HeaderMap,
    Json(req): Json<RenameReq>,
) -> Result<String, ApiError> {
    check_name(&app)?;
    let new = req.new.trim().to_string();
    check_name(&new)?;
    if new == app {
        return Err(bad_request("new name is the same as the current name"));
    }
    let actor = crate::authz::require(&state, &headers, &org, Role::Admin)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;

    // A pending render PR means env diverges from main — renaming now could
    // orphan an app that's running but only staged in that PR. Merge/close first.
    if has_open_render_pr(&state, &org)
        .await
        .map_err(bad_gateway)?
    {
        return Err(bad_request(
            "this project has an unmerged render PR — merge or close it in Deployments before renaming, so the rename migrates the current deployed state",
        ));
    }

    // Current app files (manifests + secrets), keyed by path relative to
    // `apps/<app>/`. Refuse if the app is missing or the target already exists.
    let dir = app_dir_files(&state, &org, &app)
        .await
        .map_err(bad_gateway)?;
    if dir.is_empty() {
        return Err(bad_request(format!("app {app} not found")));
    }
    if !app_dir_files(&state, &org, &new)
        .await
        .map(|d| d.is_empty())
        .unwrap_or(true)
    {
        return Err(bad_request(format!("app {new} already exists")));
    }

    // A stateful app (persistent volume or managed DB) needs its data migrated
    // to the new names — the reconciler brackets the git flip with a freeze +
    // volume/DB migration; a stateless app just blue-greens on the next converge.
    let manifests: BTreeMap<String, String> = dir
        .iter()
        .filter(|(p, _)| MANIFEST_FILES.contains(&p.as_str()))
        .map(|(p, b)| Ok((p.clone(), String::from_utf8(b.clone())?)))
        .collect::<Result<_>>()
        .map_err(bad_gateway)?;
    let manifest = merged_manifest(&app, &manifests).map_err(|e| bad_request(format!("{e:#}")))?;
    let stateful = !manifest.volumes.is_empty() || manifest.database.is_some();

    // Declared in project.yaml ⇒ it has a source repo to rename.
    let mut project = read_project(&state, &org).await.map_err(bad_gateway)?;
    let declared = project.apps.iter().any(|a| a.name == app);
    let client = state.github.org_client(&org).await.map_err(bad_gateway)?;

    // 0. Copy the app's own image(s) into the NEW GHCR package before anything
    //    flips. Renaming the repo doesn't move existing packages, so without this
    //    the rewritten pin `ghcr.io/<org>/<new>@<digest>` would 404. Done first so
    //    a missing `write:packages` token aborts the rename cleanly (nothing
    //    changed yet) rather than mid-flight.
    let mut image_digests: std::collections::BTreeSet<String> = Default::default();
    for content in manifests.values() {
        if let (_, Some(digest)) =
            rewrite_manifest_image(content, &org, &app, &new).map_err(bad_gateway)?
        {
            image_digests.insert(digest);
        }
    }
    if !image_digests.is_empty() {
        let (user, pass) = crate::proxy::ghcr_credential(&state, &org)
            .await
            .map_err(bad_gateway)?;
        for digest in &image_digests {
            crate::registry::copy_image(&state.http, &org, &app, &new, digest, &user, &pass)
                .await
                .map_err(|e| {
                    bad_gateway(anyhow::anyhow!(
                        "copying image {digest} into the {new} package \
                         (needs a write:packages GHCR token): {e:#}"
                    ))
                })?;
        }
    }

    // 1. Rename the source repo FIRST — before the ops commit flips
    //    project.yaml — so org-sync never sees the old repo undeclared.
    if declared {
        crate::org_sync::rename_repo(&client, &org, &app, &new)
            .await
            .map_err(bad_gateway)?;
    }

    // 2. One atomic ops-main commit: move the app dir (rewriting the manifest
    //    `name`; secrets move verbatim) + rewrite project.yaml apps[].name.
    let mut changes: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (rel, bytes) in &dir {
        let content = String::from_utf8(bytes.clone()).map_err(|e| bad_gateway(e.into()))?;
        let content = if MANIFEST_FILES.contains(&rel.as_str()) {
            let named = set_manifest_name(&content, &new).map_err(bad_gateway)?;
            // Keep the image name in sync with the repo (copied to the new
            // package in step 0); leaves custom/external images untouched.
            rewrite_manifest_image(&named, &org, &app, &new)
                .map_err(bad_gateway)?
                .0
        } else {
            content
        };
        changes.insert(format!("apps/{new}/{rel}"), Some(content));
        changes.insert(format!("apps/{app}/{rel}"), None);
    }
    if declared {
        for a in &mut project.apps {
            if a.name == app {
                a.name = new.clone();
            }
        }
        changes.insert(
            "project.yaml".to_string(),
            Some(serde_yaml::to_string(&project).map_err(|e| bad_gateway(e.into()))?),
        );
    }
    let message = format!("rename app {app} → {new} via dashboard by {actor}");
    commit_ops_tree(&state, &org, &changes, &message)
        .await
        .map_err(bad_gateway)?;

    // 3. Freeze the rename in the reconciler *before* the env branch flips, so
    //    the drift-poll converge can't create an empty new stack or GC the old
    //    one mid-migration. Reads the still-old env branches to find the classes.
    if stateful {
        reconciler_rename(&state, &org, "prepare", &app, &new)
            .await
            .map_err(bad_gateway)?;
    }

    // 4. The ops-main push drives the render pipeline (webhook, like a manifest
    //    edit): non-production auto-merges; wait for + merge the production
    //    render PR(s). This flips env/<class> to the new name.
    let merged = merge_render_prs(&state, &org, &app_classes(&manifests))
        .await
        .map_err(bad_gateway)?;

    // 5. Migrate the data (stop old, copy volumes, rename DB) now that the env
    //    branch has the new name, then converge onto the migrated storage.
    if stateful {
        reconciler_rename(&state, &org, "commit", &app, &new)
            .await
            .map_err(bad_gateway)?;
        crate::notify::notify_reconciler(&state, &org, "ops", "main", "rename").await;
    }

    state
        .store
        .log_event(
            "app-renamed",
            Some(&org),
            &format!("{app} → {new} by {actor}"),
        )
        .map_err(bad_gateway)?;
    Ok(format!(
        "renamed {app} → {new}{}; render propagated{}{}",
        if declared {
            " (source repo renamed)"
        } else {
            ""
        },
        if merged.is_empty() {
            String::new()
        } else {
            format!("; deployed: {}", merged.join(", "))
        },
        if stateful {
            " (data migrated — brief downtime during cutover)"
        } else {
            ""
        }
    ))
}

/// `POST /api/projects/{org}/rename` — rename a project (platform admin). The
/// project name prefixes every app's container/volume/DB, so this refreezes the
/// whole project, flips `projects.yaml` + the ops `project.yaml`, then migrates
/// each app's data to the new prefix and removes the old-prefixed containers.
/// A brief per-app cutover (the project-label change defeats blue-green GC).
pub async fn project_rename_post(
    State(state): State<Arc<AppState>>,
    Path(org): Path<String>,
    headers: HeaderMap,
    Json(req): Json<RenameReq>,
) -> Result<String, ApiError> {
    let new = req.new.trim().to_string();
    if new.is_empty()
        || !new
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(bad_request("invalid project name (lowercase, digits, -)"));
    }
    let actor = crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;

    let mut registry = read_projects(&state).await.map_err(bad_gateway)?;
    let old = registry
        .projects
        .iter()
        .find(|p| p.org == org)
        .map(|p| p.name.clone())
        .ok_or_else(|| bad_request(format!("org {org} is not a registered project")))?;
    if new == old {
        return Err(bad_request("new name is the same as the current name"));
    }
    if registry.projects.iter().any(|p| p.name == new) {
        return Err(bad_request(format!("project name {new} is already in use")));
    }
    // A pending render PR means env diverges from main — a running-but-staged
    // app would be missed by the migration (its data orphaned). Settle it first.
    if has_open_render_pr(&state, &org)
        .await
        .map_err(bad_gateway)?
    {
        return Err(bad_request(
            "this project has an unmerged render PR — merge or close it in Deployments before renaming the project, so every app migrates from its current deployed state",
        ));
    }

    // 1. Freeze every app in the project before projects.yaml flips.
    reconciler_rename(&state, &org, "project-prepare", &old, &new)
        .await
        .map_err(bad_gateway)?;

    // 2. Flip the registry name (platform repo) + the ops project.yaml name.
    for p in &mut registry.projects {
        if p.org == org {
            p.name = new.clone();
        }
    }
    let yaml = format!(
        "# Managed by the platform — project registry (§2).\n{}",
        serde_yaml::to_string(&registry).map_err(|e| bad_gateway(e.into()))?
    );
    commit_platform_file(
        &state,
        "projects.yaml",
        &yaml,
        &format!("projects: rename {old} → {new} via dashboard by {actor}"),
    )
    .await
    .map_err(bad_gateway)?;
    let mut project = read_project(&state, &org).await.map_err(bad_gateway)?;
    project.name = new.clone();
    commit_file(
        &state,
        &org,
        "project.yaml",
        &serde_yaml::to_string(&project).map_err(|e| bad_gateway(e.into()))?,
        &format!("project: rename {old} → {new} via dashboard by {actor}"),
    )
    .await
    .map_err(bad_gateway)?;

    // 3. Migrate every app's data to the new prefix + remove old containers,
    //    then converge onto the new project.
    reconciler_rename(&state, &org, "project-commit", &old, &new)
        .await
        .map_err(bad_gateway)?;
    crate::notify::notify_reconciler(&state, &org, "platform", "main", "project-rename").await;

    state
        .store
        .log_event(
            "project-renamed",
            Some(&org),
            &format!("{old} → {new} by {actor}"),
        )
        .map_err(bad_gateway)?;
    Ok(format!(
        "renamed project {old} → {new}; every app migrated to the new prefix (brief per-app cutover)"
    ))
}

/// Call one of the reconciler's imperative rename phases (`prepare` | `commit`).
async fn reconciler_rename(
    state: &AppState,
    org: &str,
    phase: &str,
    old: &str,
    new: &str,
) -> Result<()> {
    anyhow::ensure!(
        !state.config.reconciler_url.is_empty(),
        "reconciler URL not configured — cannot migrate a stateful app's data"
    );
    let url = format!(
        "{}/api/rename/{phase}/{org}",
        state.config.reconciler_url.trim_end_matches('/')
    );
    let resp = state
        .http
        .post(&url)
        .json(&serde_json::json!({ "old": old, "new": new }))
        .send()
        .await
        .with_context(|| format!("calling reconciler rename {phase}"))?;
    let ok = resp.status().is_success();
    let body = resp.text().await.unwrap_or_default();
    anyhow::ensure!(ok, "reconciler rename {phase} failed: {body}");
    Ok(())
}

/// True if the project has an open render PR (base `env/*`). Renaming with one
/// pending is unsafe: the migration enumerates apps from the `env` branches, so
/// an app that's running but only pending in an unmerged render PR would be
/// orphaned (its data left behind, the new-named stack coming up empty). Callers
/// refuse the rename until the operator merges or closes it.
async fn has_open_render_pr(state: &AppState, org: &str) -> Result<bool> {
    let client = state.github.org_client(org).await?;
    let open: serde_json::Value = client
        .get(format!("/repos/{org}/ops/pulls?state=open"), None::<&()>)
        .await?;
    Ok(open.as_array().is_some_and(|prs| {
        prs.iter().any(|pr| {
            pr["base"]["ref"]
                .as_str()
                .is_some_and(|b| b.starts_with("env/"))
        })
    }))
}

/// Every file under `apps/<app>/` on ops `main`, keyed by path relative to that
/// prefix (manifests *and* SOPS secrets, unlike `app_files`).
async fn app_dir_files(
    state: &AppState,
    org: &str,
    app: &str,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let (_, tar) = crate::proxy::fetch_snapshot(state, org, "ops", "main").await?;
    let sources = majnet_common::tarball::untar(&tar)?;
    let prefix = format!("apps/{app}/");
    Ok(sources
        .into_iter()
        .filter_map(|(p, b)| p.strip_prefix(&prefix).map(|rel| (rel.to_string(), b)))
        .collect())
}

/// Merge base ⊕ (first present overlay) into a full manifest — for inspecting
/// derived fields (volumes/database) the way render/converge would see them.
fn merged_manifest(app: &str, files: &BTreeMap<String, String>) -> Result<AppManifest> {
    let base_str = files.get("base.yaml").context("the app has no base.yaml")?;
    let base: serde_yaml::Value = serde_yaml::from_str(base_str).context("base.yaml")?;
    let overlay: serde_yaml::Value = match files.keys().find(|f| f.as_str() != "base.yaml") {
        Some(f) => serde_yaml::from_str(&files[f]).with_context(|| f.clone())?,
        None => serde_yaml::Value::Mapping(Default::default()),
    };
    let mut merged = merge(base, overlay);
    if let serde_yaml::Value::Mapping(map) = &mut merged {
        let key = serde_yaml::Value::from("name");
        if map.get(&key).is_none() {
            map.insert(key, serde_yaml::Value::from(app));
        }
    }
    AppManifest::parse(&serde_yaml::to_string(&merged)?)
}

/// Rewrite an existing top-level `name:` in a manifest YAML file. Sparse
/// overlays without a `name` are left untouched (render injects it from the
/// directory), and non-mapping/SOPS files must not be passed here.
fn set_manifest_name(yaml: &str, new: &str) -> Result<String> {
    let mut v: serde_yaml::Value = serde_yaml::from_str(yaml)?;
    if let serde_yaml::Value::Mapping(map) = &mut v {
        let key = serde_yaml::Value::from("name");
        if map.contains_key(&key) {
            map.insert(key, serde_yaml::Value::from(new));
        }
    }
    Ok(serde_yaml::to_string(&v)?)
}

/// If a manifest's `image:` points at the app's own GHCR package
/// (`ghcr.io/<org>/<old>@<digest>`), rewrite the repo path to `<new>` and return
/// the digest that must be copied into the new package. Images that don't follow
/// the convention (custom/external, or a different name) are left untouched —
/// returns `(unchanged, None)`. Sparse overlays without `image` are no-ops.
fn rewrite_manifest_image(
    yaml: &str,
    org: &str,
    old: &str,
    new: &str,
) -> Result<(String, Option<String>)> {
    let mut v: serde_yaml::Value = serde_yaml::from_str(yaml)?;
    let expected = format!("ghcr.io/{org}/{old}");
    let mut copied = None;
    if let serde_yaml::Value::Mapping(map) = &mut v {
        let key = serde_yaml::Value::from("image");
        if let Some(serde_yaml::Value::String(image)) = map.get(&key) {
            if let Some((repo, digest)) = image.split_once('@') {
                if repo == expected {
                    let rewritten = format!("ghcr.io/{org}/{new}@{digest}");
                    copied = Some(digest.to_string());
                    map.insert(key, serde_yaml::Value::from(rewritten));
                }
            }
        }
    }
    Ok((serde_yaml::to_string(&v)?, copied))
}

/// The env classes an app renders into (which overlay files it has).
fn app_classes(manifests: &BTreeMap<String, String>) -> Vec<EnvClass> {
    EnvClass::ALL
        .iter()
        .copied()
        .filter(|c| manifests.contains_key(&format!("{}.yaml", c.as_str())))
        .collect()
}

/// One atomic commit on ops `main` from a set of path changes (`Some` = write,
/// `None` = delete). Returns the new commit SHA.
async fn commit_ops_tree(
    state: &AppState,
    org: &str,
    changes: &BTreeMap<String, Option<String>>,
    message: &str,
) -> Result<String> {
    let client = state.github.org_client(org).await?;
    let repo = format!("/repos/{org}/ops");
    let head = crate::git::get_branch_head(&client, &repo, "main")
        .await?
        .context("ops has no main branch")?;
    let base_tree = crate::git::commit_tree(&client, &repo, &head).await?;
    let tree = crate::git::create_tree_incremental(&client, &repo, &base_tree, changes).await?;
    let commit = crate::git::create_commit(&client, &repo, &tree, &[&head], message).await?;
    crate::git::force_update_ref(&client, &repo, "main", &commit).await?;
    Ok(commit)
}

/// Merge the open production render PR(s) for the given classes (non-production
/// classes auto-merge at render). The render runs asynchronously off the ops
/// push, so poll for each class's PR to appear before merging. Returns the
/// classes actually deployed; a class whose PR never appears is left for the
/// operator to merge in Deployments (rather than failing the whole rename).
async fn merge_render_prs(
    state: &AppState,
    org: &str,
    classes: &[EnvClass],
) -> Result<Vec<String>> {
    let client = state.github.org_client(org).await?;
    let repo = format!("/repos/{org}/ops");
    let mut merged = Vec::new();
    for class in classes {
        if class.auto_merges() {
            continue;
        }
        match find_open_render_pr(&client, &repo, org, *class).await? {
            Some(number) => {
                merge_pr_with_retry(&client, &repo, number).await?;
                merged.push(class.as_str().to_string());
            }
            None => tracing::warn!(
                org,
                class = class.as_str(),
                "render PR did not appear in time — merge it in Deployments to finish the deploy"
            ),
        }
    }
    Ok(merged)
}

/// Poll for the open render PR of a class (`render/<class>` → `env/<class>`).
/// The render is triggered asynchronously by the ops-main push webhook, so the
/// PR appears a beat later, and GitHub's PR list is eventually consistent right
/// after creation — hence the retry.
async fn find_open_render_pr(
    client: &octocrab::Octocrab,
    repo: &str,
    org: &str,
    class: EnvClass,
) -> Result<Option<u64>> {
    let env_branch = class.env_branch();
    let render_branch = format!("render/{}", class.as_str());
    for attempt in 0..20 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let open: serde_json::Value = client
            .get(
                format!("{repo}/pulls?state=open&base={env_branch}&head={org}:{render_branch}"),
                None::<&()>,
            )
            .await?;
        if let Some(pr) = open.as_array().and_then(|p| p.first()) {
            return Ok(Some(
                pr["number"].as_u64().context("render PR has no number")?,
            ));
        }
    }
    Ok(None)
}

/// Merge a render PR, retrying while GitHub computes mergeability. A merge
/// attempted immediately after the PR is opened/updated can 405 with "not
/// mergeable" because the `mergeable` flag is still `null` (computed async);
/// the render branch is built from the env head so there's never a real
/// conflict, so we back off and retry a few times.
async fn merge_pr_with_retry(client: &octocrab::Octocrab, repo: &str, number: u64) -> Result<()> {
    let mut last: Option<octocrab::Error> = None;
    for attempt in 0..6 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let res: Result<serde_json::Value, octocrab::Error> = client
            .put(
                format!("{repo}/pulls/{number}/merge"),
                Some(&serde_json::json!({ "merge_method": "merge" })),
            )
            .await;
        match res {
            Ok(_) => return Ok(()),
            // Retry only the transient "mergeability not yet computed" case.
            Err(e) if format!("{e}").to_lowercase().contains("mergeable") => last = Some(e),
            Err(e) => return Err(e).with_context(|| format!("merging render PR #{number}")),
        }
    }
    Err(last.expect("loop set an error"))
        .with_context(|| format!("render PR #{number} still not mergeable after retries"))
}

// ── archive + permanent delete ───────────────────────────────────────────────

/// `POST /api/apps/{org}/{app}/archive` — take an app down but keep it fully
/// recoverable (admin). Moves `apps/<app>/*` → `archived/<app>/*` and drops it
/// from `project.yaml` in one commit: render un-deploys it, org-sync archives
/// the source repo, and its volumes + databases are left intact.
pub async fn app_archive_post(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    check_name(&app)?;
    let actor = crate::authz::require(&state, &headers, &org, Role::Admin)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    if has_open_render_pr(&state, &org)
        .await
        .map_err(bad_gateway)?
    {
        return Err(bad_request(
            "this project has an unmerged render PR — merge or close it in Deployments first",
        ));
    }
    let dir = app_dir_files(&state, &org, &app)
        .await
        .map_err(bad_gateway)?;
    if dir.is_empty() {
        return Err(bad_request(format!("app {app} not found")));
    }
    let manifests: BTreeMap<String, String> = dir
        .iter()
        .filter(|(p, _)| MANIFEST_FILES.contains(&p.as_str()))
        .map(|(p, b)| Ok((p.clone(), String::from_utf8(b.clone())?)))
        .collect::<Result<_>>()
        .map_err(bad_gateway)?;

    let mut changes: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (rel, bytes) in &dir {
        let content = String::from_utf8(bytes.clone()).map_err(|e| bad_gateway(e.into()))?;
        changes.insert(format!("archived/{app}/{rel}"), Some(content));
        changes.insert(format!("apps/{app}/{rel}"), None);
    }
    let mut project = read_project(&state, &org).await.map_err(bad_gateway)?;
    if project.apps.iter().any(|a| a.name == app) {
        project.apps.retain(|a| a.name != app);
        changes.insert(
            "project.yaml".to_string(),
            Some(serde_yaml::to_string(&project).map_err(|e| bad_gateway(e.into()))?),
        );
    }
    commit_ops_tree(
        &state,
        &org,
        &changes,
        &format!("archive app {app} via dashboard by {actor}"),
    )
    .await
    .map_err(bad_gateway)?;
    merge_render_prs(&state, &org, &app_classes(&manifests))
        .await
        .map_err(bad_gateway)?;
    state
        .store
        .log_event("app-archived", Some(&org), &format!("{app} by {actor}"))
        .map_err(bad_gateway)?;
    Ok(format!(
        "archived {app} — undeployed and source repo archived; data kept. Permanently delete it from the project page to reclaim storage."
    ))
}

/// `GET /api/archived/{org}` — names of archived apps (`archived/<app>/`).
pub async fn archived_get(
    State(state): State<Arc<AppState>>,
    Path(org): Path<String>,
) -> Result<Json<Vec<String>>, ApiError> {
    let (_, tar) = crate::proxy::fetch_snapshot(&state, &org, "ops", "main")
        .await
        .map_err(bad_gateway)?;
    let files = majnet_common::tarball::untar(&tar).map_err(bad_gateway)?;
    let mut names: Vec<String> = files
        .keys()
        .filter_map(|p| p.strip_prefix("archived/"))
        .filter_map(|rest| rest.strip_suffix("/base.yaml"))
        .map(str::to_string)
        .collect();
    names.sort();
    Ok(Json(names))
}

/// `POST /api/apps/{org}/{app}/delete` — permanently delete an ARCHIVED app
/// (admin). Purges its runtime + volumes + databases (reconciler), deletes the
/// GitHub source repo, and removes the archived manifests. Irreversible.
pub async fn app_delete_post(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    check_name(&app)?;
    let actor = crate::authz::require(&state, &headers, &org, Role::Admin)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let (_, tar) = crate::proxy::fetch_snapshot(&state, &org, "ops", "main")
        .await
        .map_err(bad_gateway)?;
    let files = majnet_common::tarball::untar(&tar).map_err(bad_gateway)?;
    let prefix = format!("archived/{app}/");
    if !files.keys().any(|p| *p == format!("{prefix}base.yaml")) {
        return Err(bad_request(format!(
            "{app} is not archived — archive it first"
        )));
    }
    if files.keys().any(|p| p.starts_with(&format!("apps/{app}/"))) {
        return Err(bad_request(format!("{app} still has active manifests")));
    }

    // 1. Reap runtime + volumes + databases.
    reconciler_purge(&state, &org, &app)
        .await
        .map_err(bad_gateway)?;
    // 2. Delete the source repo (graceful — a manifests-only app has none, and
    //    org policy may block deletion; in either case keep going).
    let repo_note = match delete_repo(&state, &org, &app).await {
        Ok(true) => "source repo deleted",
        Ok(false) => "no source repo",
        Err(e) => {
            tracing::warn!(org, app, error = format!("{e:#}"), "repo delete failed");
            "source repo left archived (deletion blocked)"
        }
    };
    // 3. Remove the archived manifests.
    let changes: BTreeMap<String, Option<String>> = files
        .keys()
        .filter(|p| p.starts_with(&prefix))
        .map(|p| (p.clone(), None))
        .collect();
    commit_ops_tree(
        &state,
        &org,
        &changes,
        &format!("delete archived app {app} via dashboard by {actor}"),
    )
    .await
    .map_err(bad_gateway)?;
    state
        .store
        .log_event("app-deleted", Some(&org), &format!("{app} by {actor}"))
        .map_err(bad_gateway)?;
    Ok(format!(
        "permanently deleted {app}: purged containers, volumes and databases; {repo_note}"
    ))
}

/// Ask the reconciler to purge an archived app's runtime + data.
async fn reconciler_purge(state: &AppState, org: &str, app: &str) -> Result<()> {
    anyhow::ensure!(
        !state.config.reconciler_url.is_empty(),
        "reconciler URL not configured — cannot purge app data"
    );
    let url = format!(
        "{}/api/purge/{org}",
        state.config.reconciler_url.trim_end_matches('/')
    );
    let resp = state
        .http
        .post(&url)
        .json(&serde_json::json!({ "app": app }))
        .send()
        .await
        .context("calling reconciler purge")?;
    let ok = resp.status().is_success();
    let body = resp.text().await.unwrap_or_default();
    anyhow::ensure!(ok, "reconciler purge failed: {body}");
    Ok(())
}

/// Delete a source repo. `Ok(true)` = deleted, `Ok(false)` = no such repo
/// (manifests-only app or already gone), `Err` = deletion refused (org policy).
async fn delete_repo(state: &AppState, org: &str, repo: &str) -> Result<bool> {
    let client = state.github.org_client(org).await?;
    let exists: std::result::Result<serde_json::Value, _> = client
        .get(format!("/repos/{org}/{repo}"), None::<&()>)
        .await;
    if exists.is_err() {
        return Ok(false);
    }
    client
        .delete::<serde_json::Value, _, ()>(format!("/repos/{org}/{repo}"), None)
        .await
        .with_context(|| format!("deleting repo {org}/{repo}"))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "image: ghcr.io/x/app@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nhealth:\n  path: /\n  port: 80\n";

    #[test]
    fn valid_base_plus_overlay_passes() {
        let files = BTreeMap::from([
            ("base.yaml".to_string(), BASE.to_string()),
            ("stable.yaml".to_string(), "env:\n  X: \"1\"\n".to_string()),
        ]);
        validate_app_files("myapp", &files).unwrap();
    }

    #[test]
    fn rewrite_manifest_image_follows_the_repo() {
        let d = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        // Matches ghcr.io/<org>/<old> → rewritten, digest returned for copy.
        let (out, copied) =
            rewrite_manifest_image(&format!("image: ghcr.io/o/old@{d}\n"), "o", "old", "new")
                .unwrap();
        assert_eq!(copied.as_deref(), Some(d));
        assert!(out.contains(&format!("ghcr.io/o/new@{d}")));

        // A custom/external image name is left untouched.
        let (out, copied) = rewrite_manifest_image(
            &format!("image: ghcr.io/o/custom-name@{d}\n"),
            "o",
            "old",
            "new",
        )
        .unwrap();
        assert!(copied.is_none());
        assert!(out.contains(&format!("ghcr.io/o/custom-name@{d}")));

        // A sparse overlay without an image is a no-op.
        let (_, copied) = rewrite_manifest_image("env:\n  X: \"1\"\n", "o", "old", "new").unwrap();
        assert!(copied.is_none());
    }

    #[test]
    fn tag_pinned_image_is_rejected() {
        let files = BTreeMap::from([
            (
                "base.yaml".to_string(),
                "image: ghcr.io/x/app:latest\nhealth:\n  path: /\n  port: 80\n".to_string(),
            ),
            ("stable.yaml".to_string(), "{}\n".to_string()),
        ]);
        let err = validate_app_files("myapp", &files).unwrap_err();
        assert!(format!("{err:#}").contains("digest-pinned"), "{err:#}");
    }

    #[test]
    fn overlay_without_base_is_rejected() {
        let files = BTreeMap::from([("stable.yaml".to_string(), "{}\n".to_string())]);
        assert!(validate_app_files("myapp", &files).is_err());
    }

    #[test]
    fn base_without_any_overlay_is_rejected() {
        let files = BTreeMap::from([("base.yaml".to_string(), BASE.to_string())]);
        assert!(validate_app_files("myapp", &files).is_err());
    }

    fn new_app(classes: &[&str]) -> NewApp {
        NewApp {
            name: "blog".into(),
            image: "ghcr.io/x/blog@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            host: "blog.example.com".into(),
            port: 8080,
            domains: vec!["www.example.com".into()],
            classes: classes.iter().map(|s| s.to_string()).collect(),
            database: Some("postgres".into()),
            template: "web-app".into(),
            create_repo: true,
            import: None,
        }
    }

    #[test]
    fn scaffold_produces_a_valid_manifest() {
        let base = scaffold_base(&new_app(&["production"])).unwrap();
        let m = AppManifest::parse(&base).unwrap();
        assert_eq!(m.name, "blog");
        let ingress = m.ingress.unwrap();
        assert_eq!(ingress.host.as_deref(), Some("blog.example.com"));
        assert_eq!(ingress.hosts(), vec!["blog.example.com", "www.example.com"]);
        assert!(m.database.is_some());
        // And it validates as base ⊕ empty-overlay the way apps_post commits it.
        let files = BTreeMap::from([
            ("base.yaml".to_string(), base),
            ("production.yaml".to_string(), "{}\n".to_string()),
        ]);
        validate_app_files("blog", &files).unwrap();
    }

    #[test]
    fn scaffold_without_a_port_omits_ingress() {
        let mut req = new_app(&["stable"]);
        req.host = String::new();
        req.domains.clear();
        req.port = 0;
        req.database = None;
        let base = scaffold_base(&req).unwrap();
        let m = AppManifest::parse(&base).unwrap();
        assert!(m.ingress.is_none() && m.database.is_none());
    }

    #[test]
    fn scaffold_port_only_emits_hostless_ingress() {
        // ADR 0013: a port opts into routing; non-production hosts are
        // auto-assigned at render, so no host is written to base.yaml.
        let mut req = new_app(&["stable"]);
        req.host = String::new();
        req.domains = vec!["www.example.com".into()]; // ignored without a host
        let base = scaffold_base(&req).unwrap();
        let m = AppManifest::parse(&base).unwrap();
        let ingress = m.ingress.expect("a port opts into ingress");
        assert_eq!(ingress.host, None);
        assert_eq!(ingress.port, 8080);
        assert!(ingress.domains.is_empty());
    }

    #[test]
    fn scaffold_rejects_a_tag_pinned_image() {
        let mut req = new_app(&["stable"]);
        req.image = "ghcr.io/x/blog:latest".into();
        assert!(scaffold_base(&req).is_err());
    }

    #[test]
    fn set_manifest_name_rewrites_only_existing_name() {
        // base.yaml carries a name → rewritten, order + other keys preserved.
        let out = set_manifest_name("name: old\nimage: x\n", "new").unwrap();
        let v: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(v["name"], serde_yaml::Value::from("new"));
        assert_eq!(v["image"], serde_yaml::Value::from("x"));
        // A sparse overlay with no name is left without one (render injects it).
        let empty = set_manifest_name("{}\n", "new").unwrap();
        let ev: serde_yaml::Value = serde_yaml::from_str(&empty).unwrap();
        assert!(ev.get("name").is_none());
    }

    #[test]
    fn app_classes_reads_overlays_present() {
        let files = BTreeMap::from([
            ("base.yaml".to_string(), BASE.to_string()),
            ("production.yaml".to_string(), "{}\n".to_string()),
            ("stable.yaml".to_string(), "{}\n".to_string()),
        ]);
        let classes = app_classes(&files);
        assert!(classes.contains(&EnvClass::Production));
        assert!(classes.contains(&EnvClass::Stable));
        assert!(!classes.contains(&EnvClass::Ephemeral));
    }

    #[test]
    fn merged_manifest_surfaces_volumes_and_database() {
        let files = BTreeMap::from([
            (
                "base.yaml".to_string(),
                format!(
                    "{BASE}volumes:\n  - name: data\n    path: /d\ndatabase:\n  engine: postgres\n"
                ),
            ),
            ("production.yaml".to_string(), "{}\n".to_string()),
        ]);
        let m = merged_manifest("app", &files).unwrap();
        assert!(!m.volumes.is_empty());
        assert!(m.database.is_some());
    }

    #[test]
    fn summarize_reads_image_hosts_and_classes() {
        let base = scaffold_base(&new_app(&["production", "stable"])).unwrap();
        let text = BTreeMap::from([
            ("apps/blog/base.yaml".to_string(), base),
            ("apps/blog/production.yaml".to_string(), "{}\n".to_string()),
            ("apps/blog/stable.yaml".to_string(), "{}\n".to_string()),
        ]);
        let s = summarize_app("blog", &text).unwrap();
        assert_eq!(s.name, "blog");
        assert_eq!(s.classes, vec!["production", "stable"]);
        assert_eq!(s.host.as_deref(), Some("blog.example.com"));
        assert_eq!(s.domains, vec!["blog.example.com", "www.example.com"]);
        assert_eq!(s.database.as_deref(), Some("postgres"));
    }
}
