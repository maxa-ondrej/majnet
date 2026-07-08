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
use majnet_common::project::{Member, ProjectConfig, Role};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::AppState;

type ApiError = (StatusCode, String);

fn bad_gateway(e: anyhow::Error) -> ApiError {
    (StatusCode::BAD_GATEWAY, format!("{e:#}"))
}
fn bad_request(msg: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, msg.into())
}

const MANIFEST_FILES: [&str; 4] = [
    "base.yaml",
    "stable.yaml",
    "production.yaml",
    "ephemeral.yaml",
];

/// `GET /api/manifest/{org}/{app}` — the app's manifest files on ops `main`.
pub async fn manifest_get(
    State(state): State<Arc<AppState>>,
    Path((org, app)): Path<(String, String)>,
) -> Result<Json<BTreeMap<String, String>>, ApiError> {
    check_name(&app)?;
    let files = app_files(&state, &org, &app).await.map_err(bad_gateway)?;
    Ok(Json(files))
}

/// `PUT /api/manifest/{org}/{app}/{file}` — validate + commit one manifest
/// file. Body is the raw YAML.
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
async fn app_files(state: &AppState, org: &str, app: &str) -> Result<BTreeMap<String, String>> {
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
fn validate_app_files(app: &str, files: &BTreeMap<String, String>) -> Result<()> {
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
async fn commit_file(
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

async fn read_project(state: &AppState, org: &str) -> Result<ProjectConfig> {
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
pub async fn whoami(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<WhoAmI>, ApiError> {
    use majnet_common::authz::Actor;
    let actor = crate::authz::actor(&state, &headers)
        .await
        .map_err(bad_gateway)?;
    Ok(Json(match actor {
        Actor::Infra => WhoAmI {
            login: None,
            admin: true,
        },
        Actor::Human {
            github,
            platform_admin,
        } => WhoAmI {
            login: Some(github),
            admin: platform_admin,
        },
    }))
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
    let message = format!("projects: register {} ({}) via dashboard by {actor}", req.name, req.org);
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
            Err(e) => tracing::warn!(org, name, error = format!("{e:#}"), "skipping unparsable app"),
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct NewApp {
    pub name: String,
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
}

/// `POST /api/apps/{org}` — scaffold a new app's `base.yaml` + selected class
/// overlays on ops `main`. Creating a production overlay is an admin action.
pub async fn apps_post(
    State(state): State<Arc<AppState>>,
    Path(org): Path<String>,
    headers: HeaderMap,
    Json(req): Json<NewApp>,
) -> Result<String, ApiError> {
    check_name(&req.name)?;
    let valid_classes = ["stable", "production", "ephemeral"];
    if req.classes.is_empty() || !req.classes.iter().all(|c| valid_classes.contains(&c.as_str())) {
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

    // Refuse to clobber an existing app.
    let mut files = app_files(&state, &org, &req.name).await.map_err(bad_gateway)?;
    if !files.is_empty() {
        return Err(bad_request(format!("app {} already exists", req.name)));
    }

    let base = scaffold_base(&req).map_err(|e| bad_request(format!("{e:#}")))?;
    files.insert("base.yaml".to_string(), base.clone());
    for class in &req.classes {
        files.insert(format!("{class}.yaml"), "{}\n".to_string());
    }
    validate_app_files(&req.name, &files).map_err(|e| bad_request(format!("{e:#}")))?;

    // Commit base first, then each overlay, so the app renders atomically enough.
    commit_file(
        &state,
        &org,
        &format!("apps/{}/base.yaml", req.name),
        &base,
        &format!("manifest({}): scaffold via dashboard by {actor}", req.name),
    )
    .await
    .map_err(bad_gateway)?;
    for class in &req.classes {
        commit_file(
            &state,
            &org,
            &format!("apps/{}/{class}.yaml", req.name),
            "{}\n",
            &format!("manifest({}): add {class} overlay via dashboard by {actor}", req.name),
        )
        .await
        .map_err(bad_gateway)?;
    }
    state
        .store
        .log_event(
            "app-scaffolded",
            Some(&org),
            &format!("{} [{}] by {actor}", req.name, req.classes.join(",")),
        )
        .map_err(bad_gateway)?;
    Ok(format!(
        "{} scaffolded ({}); render PRs will propagate",
        req.name,
        req.classes.join(", ")
    ))
}

// ── nodes ────────────────────────────────────────────────────────────────────

/// `GET /api/nodes` — the platform's node registry (`nodes.yaml`).
pub async fn nodes_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Node>>, ApiError> {
    let files = read_platform(&state).await.map_err(bad_gateway)?;
    let yaml = files
        .get("nodes.yaml")
        .ok_or_else(|| bad_gateway(anyhow::anyhow!("platform repo has no nodes.yaml")))?;
    let nodes = NodesFile::parse(yaml.as_bytes()).map_err(bad_gateway)?;
    Ok(Json(nodes.nodes))
}

// ── helpers (projects/apps/nodes) ─────────────────────────────────────────────

/// Untarred platform-repo `main` snapshot, path → UTF-8 text.
async fn read_platform(state: &AppState) -> Result<BTreeMap<String, String>> {
    let (_, tar) =
        crate::proxy::fetch_snapshot(state, &state.config.root_org, "platform", "main").await?;
    let files = majnet_common::tarball::untar(&tar)?;
    Ok(files
        .into_iter()
        .filter_map(|(p, b)| String::from_utf8(b).ok().map(|s| (p, s)))
        .collect())
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
    let mut classes: Vec<String> = ["stable", "production", "ephemeral"]
        .into_iter()
        .filter(|c| text.contains_key(&format!("{prefix}{c}.yaml")))
        .map(str::to_string)
        .collect();
    classes.sort();

    let overlay: serde_yaml::Value = match classes.first() {
        Some(c) => serde_yaml::from_str(
            text.get(&format!("{prefix}{c}.yaml")).context("overlay")?,
        )?,
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
        host: manifest.ingress.as_ref().map(|i| i.host.clone()),
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
        yaml.push_str(&format!("ingress:\n  host: {}\n  port: {}\n", req.host, req.port));
        let extra: Vec<&String> = req.domains.iter().filter(|d| !d.is_empty()).collect();
        if !extra.is_empty() {
            yaml.push_str("  domains:\n");
            for d in extra {
                yaml.push_str(&format!("    - {d}\n"));
            }
        }
        yaml.push_str(&format!("health:\n  path: /\n  port: {}\n", req.port));
    }
    if let Some(engine) = req.database.as_deref().filter(|e| !e.is_empty() && *e != "none") {
        yaml.push_str(&format!("database:\n  engine: {engine}\n"));
    }
    // Validate the base in isolation the way an overlay-merged manifest would be.
    AppManifest::parse(&yaml).context("scaffolded base.yaml is invalid")?;
    Ok(yaml)
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
        }
    }

    #[test]
    fn scaffold_produces_a_valid_manifest() {
        let base = scaffold_base(&new_app(&["production"])).unwrap();
        let m = AppManifest::parse(&base).unwrap();
        assert_eq!(m.name, "blog");
        let ingress = m.ingress.unwrap();
        assert_eq!(ingress.host, "blog.example.com");
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
    fn scaffold_without_a_domain_omits_ingress() {
        let mut req = new_app(&["stable"]);
        req.host = String::new();
        req.domains.clear();
        req.database = None;
        let base = scaffold_base(&req).unwrap();
        let m = AppManifest::parse(&base).unwrap();
        assert!(m.ingress.is_none() && m.database.is_none());
    }

    #[test]
    fn scaffold_rejects_a_tag_pinned_image() {
        let mut req = new_app(&["stable"]);
        req.image = "ghcr.io/x/blog:latest".into();
        assert!(scaffold_base(&req).is_err());
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
