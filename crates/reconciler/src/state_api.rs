//! WG-internal API (§12.8): the bot's deploy nudge + read-only state for the
//! dashboard. The phase-5 restart escape hatch (§16) will live here too.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::AppState;

/// Data-restore uploads carry a whole DB dump — allow up to 2 GiB (axum
/// defaults to 2 MiB). WG-internal + operator-driven, so this is a ceiling, not
/// an exposure.
const MAX_DUMP_BYTES: usize = 2 * 1024 * 1024 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/notify", post(notify))
        .route("/api/events", get(events))
        .route("/api/restart/{project}/{class}/{app}", post(restart))
        .route("/api/secrets/{project}/{class}/{app}", get(secrets_get))
        .route("/api/metrics", get(metrics_get))
        .route("/api/metrics/history", get(metrics_history_get))
        .route("/api/metrics/container-history", get(container_history_get))
        .route("/api/logs/{project}/{class}/{app}", get(logs_get))
        .route("/api/terminal", get(crate::terminal::terminal_ws))
        .route("/api/terminal/sessions", get(crate::terminal::sessions_get))
        .route(
            "/api/terminal/transcript/{id}",
            get(crate::terminal::transcript_get),
        )
        .route("/api/info/{org}/{app}", get(info_get))
        .route("/api/settings/alerts", get(alerts_get).post(alerts_set))
        .route("/api/settings/alerts/test", post(alerts_test))
        .route("/api/rename/prepare/{org}", post(rename_prepare))
        .route("/api/rename/commit/{org}", post(rename_commit))
        .route("/api/rename/project-prepare/{org}", post(project_prepare))
        .route("/api/rename/project-commit/{org}", post(project_commit))
        .route("/api/purge/{org}", post(purge))
        .route("/api/purge-project/{org}", post(purge_project))
        .route("/api/ephemeral/extend/{project}/{app}", post(extend))
        .route(
            "/api/migrate/{project}/{app}",
            post(migrate).route_layer(DefaultBodyLimit::max(MAX_DUMP_BYTES)),
        )
        .with_state(state)
}

#[derive(serde::Deserialize)]
struct MigrateQuery {
    /// Target env class (`production` | `stable` | `testing` | `ephemeral`).
    class: String,
    /// DB engine (`postgres` | `mariadb`).
    engine: String,
}

/// `POST /api/migrate/{project}/{app}?class=&engine=` — restore a DB dump (raw
/// request body) into the app's managed database (ADR 0010 phase 3). Trust is
/// the WG bind, operator-driven, like the bot's snapshot API; the
/// maintenance-window cutover coordinates it. Idempotent via the store.
async fn migrate(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((project, app)): axum::extract::Path<(String, String)>,
    Query(q): Query<MigrateQuery>,
    dump: Bytes,
) -> Result<String, (StatusCode, String)> {
    let class: majnet_common::EnvClass = serde_yaml::from_str(&q.class).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "class must be production|stable|testing|ephemeral".into(),
        )
    })?;
    let engine: majnet_common::manifest::DbEngine =
        serde_yaml::from_str(&q.engine).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "engine must be postgres|mariadb (v1)".into(),
            )
        })?;
    crate::migrate::restore_db(&state, &project, &app, class, engine, &dump)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

/// The one imperative escape hatch (§16): restart isn't a state change, so it
/// can't be a commit. Role-gated (production = project admin, the rest =
/// developer) and audit-logged. Nothing else is imperative.
async fn restart(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((project, class, app)): axum::extract::Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<String, (StatusCode, String)> {
    let class: majnet_common::EnvClass = serde_yaml::from_str(&class).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "class must be production|stable|ephemeral".into(),
        )
    })?;
    let min_role = if class == majnet_common::EnvClass::Production {
        majnet_common::project::Role::Admin
    } else {
        majnet_common::project::Role::Developer
    };
    let actor = crate::authz::require(&state, &headers, &project, min_role)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    do_restart(&state, &project, class, &app, &actor)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

#[derive(serde::Deserialize)]
struct RenameBody {
    old: String,
    new: String,
}

/// `POST /api/rename/prepare/{org}` — freeze the rename (convergence + GC skip
/// the app) before the bot flips git. Platform-admin (infra) gated.
async fn rename_prepare(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(org): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    Json(b): Json<RenameBody>,
) -> Result<String, (StatusCode, String)> {
    crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let classes = crate::rename::prepare(&state, &org, &b.old, &b.new)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(format!(
        "froze {} → {} for [{}]",
        b.old,
        b.new,
        classes.join(", ")
    ))
}

/// `POST /api/rename/commit/{org}` — migrate the data (volumes + DB) for every
/// frozen class and clear the freeze. Run after the git flip updated the env
/// branches. Platform-admin (infra) gated.
async fn rename_commit(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(org): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    Json(b): Json<RenameBody>,
) -> Result<String, (StatusCode, String)> {
    crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let done = crate::rename::commit(&state, &org, &b.old, &b.new)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(format!(
        "migrated {} → {} for [{}]",
        b.old,
        b.new,
        done.join(", ")
    ))
}

/// `POST /api/rename/project-prepare/{org}` — freeze every app of a project
/// before its projects.yaml name flips. Body `{old, new}` = old/new project name.
async fn project_prepare(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(org): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    Json(b): Json<RenameBody>,
) -> Result<String, (StatusCode, String)> {
    crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let classes = crate::rename::project_prepare(&state, &org, &b.old, &b.new)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(format!(
        "froze project {} → {} [{}]",
        b.old,
        b.new,
        classes.join(", ")
    ))
}

/// `POST /api/rename/project-commit/{org}` — migrate every app's data to the new
/// project prefix and remove the old-prefixed containers. After the git flip.
async fn project_commit(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(org): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    Json(b): Json<RenameBody>,
) -> Result<String, (StatusCode, String)> {
    crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let done = crate::rename::project_commit(&state, &org, &b.old, &b.new)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(format!(
        "migrated project {} → {} [{}]",
        b.old,
        b.new,
        done.join(", ")
    ))
}

#[derive(serde::Deserialize)]
struct PurgeBody {
    app: String,
}

/// `POST /api/purge/{org}` — permanently reap an archived app's runtime + data
/// (containers, volumes, database). Platform-admin (infra) gated. The bot has
/// already parked the manifests under `archived/<app>/` and gates this on the
/// app being archived; here we only touch the physical footprint.
async fn purge(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(org): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    Json(b): Json<PurgeBody>,
) -> Result<String, (StatusCode, String)> {
    crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let purged = crate::purge::purge_app(&state, &org, &b.app)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(format!("purged {} [{}]", b.app, purged.join(", ")))
}

/// `POST /api/purge-project/{org}` — permanently reap an ENTIRE archived project:
/// every app's runtime + data, then the per-project network, ingress, and DB
/// role. Platform-admin (infra) gated. The bot has already emptied `project.yaml`
/// and archived every app before calling this.
async fn purge_project(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(org): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<String, (StatusCode, String)> {
    crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let purged = crate::purge::purge_project(&state, &org)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(format!(
        "purged project {org} — apps [{}] + network/ingress/role",
        purged.join(", ")
    ))
}

#[derive(serde::Serialize)]
struct AlertSettings {
    webhook_set: bool,
    cpu_pct: f64,
    mem_pct: f64,
}

async fn alerts_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<AlertSettings>, (StatusCode, String)> {
    let cfg = |k: &str, d: f64| {
        state
            .store
            .get_config(k)
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(d)
    };
    let webhook_set = state
        .store
        .get_config("alert_webhook")
        .ok()
        .flatten()
        .is_some_and(|s| !s.trim().is_empty());
    Ok(Json(AlertSettings {
        webhook_set,
        cpu_pct: cfg("alert_cpu_pct", 90.0),
        mem_pct: cfg("alert_mem_pct", 90.0),
    }))
}

#[derive(serde::Deserialize)]
struct AlertSettingsReq {
    /// Discord webhook URL. Omitted = leave unchanged; empty string = disable.
    webhook: Option<String>,
    cpu_pct: Option<f64>,
    mem_pct: Option<f64>,
}

async fn alerts_set(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<AlertSettingsReq>,
) -> Result<String, (StatusCode, String)> {
    crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let set = |k: &str, v: &str| {
        state
            .store
            .set_config(k, v)
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
    };
    if let Some(w) = req.webhook {
        let w = w.trim();
        if !w.is_empty() && !w.starts_with("https://") {
            return Err((
                StatusCode::BAD_REQUEST,
                "webhook must be an https URL".into(),
            ));
        }
        set("alert_webhook", w)?;
    }
    if let Some(c) = req.cpu_pct {
        set("alert_cpu_pct", &c.to_string())?;
    }
    if let Some(m) = req.mem_pct {
        set("alert_mem_pct", &m.to_string())?;
    }
    Ok("alert settings saved".into())
}

async fn alerts_test(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<String, (StatusCode, String)> {
    crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    crate::alerts::send_test(&state)
        .await
        .map(|_| "test message sent".into())
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

/// `GET /api/metrics` — node + container metrics for every node, gathered live
/// over the per-node Docker API. Read-only, VPN-gated (like `/api/events`).
async fn metrics_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<crate::metrics::NodeMetrics>>, (StatusCode, String)> {
    crate::metrics::gather(&state)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

#[derive(serde::Deserialize)]
struct HistoryQuery {
    /// Look-back window in seconds (default 24h, clamped 5min–60d).
    range: Option<i64>,
    /// Restrict to a single node.
    node: Option<String>,
}

/// `GET /api/metrics/history?range=<sec>&node=<name>` — persisted node/host
/// samples (ADR 0017), oldest first, already at the resolution for their age.
async fn metrics_history_get(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<Vec<crate::state::MetricPoint>>, (StatusCode, String)> {
    let range = q.range.unwrap_or(86_400).clamp(300, 60 * 86_400);
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        - range;
    state
        .store
        .metric_history(q.node.as_deref(), since)
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
}

#[derive(serde::Deserialize)]
struct ContainerHistoryQuery {
    range: Option<i64>,
    /// The container name to chart (as it appears in `/api/metrics` `apps[].name`).
    container: String,
}

/// `GET /api/metrics/container-history?range=<sec>&container=<name>` — persisted
/// per-container samples (ADR 0017 follow-up), oldest first.
async fn container_history_get(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ContainerHistoryQuery>,
) -> Result<Json<Vec<crate::state::ContainerPoint>>, (StatusCode, String)> {
    let range = q.range.unwrap_or(86_400).clamp(300, 60 * 86_400);
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        - range;
    state
        .store
        .container_history(&q.container, since)
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
}

#[derive(serde::Deserialize)]
struct LogsQuery {
    tail: Option<String>,
}

/// `GET /api/logs/{project}/{class}/{app}?tail=N` — recent container logs for an
/// app, fetched over the node's Docker API. Production is admin-gated (logs can
/// contain sensitive output); other classes developer-gated.
async fn logs_get(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((project, class, app)): axum::extract::Path<(String, String, String)>,
    Query(q): Query<LogsQuery>,
    headers: axum::http::HeaderMap,
) -> Result<String, (StatusCode, String)> {
    let class_e: majnet_common::EnvClass = serde_yaml::from_str(&class).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "class must be production|stable|testing|ephemeral".into(),
        )
    })?;
    let min_role = if class_e == majnet_common::EnvClass::Production {
        majnet_common::project::Role::Admin
    } else {
        majnet_common::project::Role::Developer
    };
    crate::authz::require(&state, &headers, &project, min_role)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;

    logs_inner(
        &state,
        &project,
        class_e,
        &app,
        q.tail.as_deref().unwrap_or("300"),
    )
    .await
    .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn logs_inner(
    state: &AppState,
    project: &str,
    class: majnet_common::EnvClass,
    app: &str,
    tail: &str,
) -> anyhow::Result<String> {
    use anyhow::Context;
    use futures_util::StreamExt;
    let platform = crate::snapshot::fetch(
        &state.http,
        &state.config,
        &state.config.root_org,
        "platform",
        "main",
    )
    .await?
    .context("platform snapshot unavailable")?;
    let nodes = majnet_common::platform::NodesFile::parse(
        platform.files.get("nodes.yaml").context("no nodes.yaml")?,
    )?;
    let node = nodes
        .by_role(class.node_role())
        .context("no node for class")?;
    let docker = state.nodes(&nodes).client_for(node).await?;

    // Container labels use the project NAME; the dashboard passes the org (they
    // differ, e.g. org majksa-projects → project demo). Resolve via projects.yaml.
    let proj_name = platform
        .files
        .get("projects.yaml")
        .and_then(|b| serde_yaml::from_slice::<majnet_common::platform::ProjectsFile>(b).ok())
        .and_then(|pf| pf.projects.into_iter().find(|p| p.org == project))
        .map(|p| p.name)
        .unwrap_or_else(|| project.to_string());

    let filters = std::collections::HashMap::from([(
        "label".to_string(),
        vec![
            format!("{}={}", crate::deploy::LABEL_PROJECT, proj_name),
            format!("{}={}", crate::deploy::LABEL_APP, app),
            format!("{}={}", crate::deploy::LABEL_CLASS, class.as_str()),
        ],
    )]);
    let list = docker
        .list_containers(Some(bollard::query_parameters::ListContainersOptions {
            all: true,
            filters: Some(filters),
            ..Default::default()
        }))
        .await?;
    let container = list
        .into_iter()
        .find_map(|c| c.id)
        .context("no container found for this app/class")?;

    let mut stream = docker.logs(
        &container,
        Some(bollard::query_parameters::LogsOptions {
            stdout: true,
            stderr: true,
            tail: tail.to_string(),
            timestamps: true,
            follow: false,
            ..Default::default()
        }),
    );
    let mut buf = String::new();
    while let Some(item) = stream.next().await {
        if let Ok(out) = item {
            buf.push_str(&out.to_string());
        }
    }
    Ok(buf)
}

/// `GET /api/info/{org}/{app}` — build metadata (version/commit/etc.) each env
/// reported at its last deploy via the standard `/info` endpoint. Read from the
/// state DB (scraped at deploy time, not probed live). Developer-gated — this is
/// non-sensitive build info, not secrets or logs.
async fn info_get(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((org, app)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Vec<crate::state::AppInfo>>, (StatusCode, String)> {
    crate::authz::require(
        &state,
        &headers,
        &org,
        majnet_common::project::Role::Developer,
    )
    .await
    .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    // Rows are keyed by project NAME (matching container labels); the dashboard
    // passes the org, which can differ (e.g. org majksa-projects → project demo).
    let project = crate::rename::resolve_project(&state, &org)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    state
        .store
        .app_info_for(&project, &app)
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
}

/// `GET /api/secrets/{project}/{class}/{app}` — decrypt and return an app's
/// current secret values for the dashboard editor. Reads the SOPS source of
/// truth `apps/{app}/secrets.{class}.yaml` from ops `main` and decrypts it with
/// the class age key. Production is admin-gated. This is the one place secret
/// plaintext leaves the reconciler for a reader (the VPN-only dashboard); every
/// other path keeps it write-only (§14).
async fn secrets_get(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((project, class, app)): axum::extract::Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<Json<BTreeMap<String, String>>, (StatusCode, String)> {
    let class_e: majnet_common::EnvClass = serde_yaml::from_str(&class).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "class must be production|stable|testing|ephemeral".into(),
        )
    })?;
    let min_role = if class_e == majnet_common::EnvClass::Production {
        majnet_common::project::Role::Admin
    } else {
        majnet_common::project::Role::Developer
    };
    crate::authz::require(&state, &headers, &project, min_role)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;

    let snap = crate::snapshot::fetch(&state.http, &state.config, &project, "ops", "main")
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    let Some(snap) = snap else {
        return Ok(Json(BTreeMap::new()));
    };
    let Some(enc) = snap.files.get(&format!("apps/{app}/secrets.{class}.yaml")) else {
        return Ok(Json(BTreeMap::new())); // no secrets set for this class yet
    };
    let values = crate::secrets::decrypt(&state.config, class_e, enc)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(Json(values))
}

/// Dashboard TTL extension (§8): postpone a preview's GC. State-adjacent but
/// not config — the manifest still owns existence; this only defers cleanup.
async fn extend(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((project, app)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: Json<ExtendRequest>,
) -> Result<String, (StatusCode, String)> {
    let days = body.days.clamp(1, 7);
    let actor = crate::authz::require(
        &state,
        &headers,
        &project,
        majnet_common::project::Role::Developer,
    )
    .await
    .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let until = state
        .store
        .ephemeral_extend(&project, &app, days)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    state
        .store
        .record(
            "imperative",
            &project,
            "-",
            &format!("extend-ttl {app} +{days}d"),
            &format!("until {until} by {actor}"),
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    tracing::info!(project, app, days, actor, "ephemeral TTL extended");
    Ok(format!("{project}/{app} protected from GC until {until}"))
}

#[derive(serde::Deserialize)]
struct ExtendRequest {
    days: u32,
}

async fn do_restart(
    state: &AppState,
    project: &str,
    class: majnet_common::EnvClass,
    app: &str,
    actor: &str,
) -> anyhow::Result<String> {
    use anyhow::Context;

    // Resolve the node the same way convergence does.
    let platform = crate::snapshot::fetch(
        &state.http,
        &state.config,
        &state.config.root_org,
        "platform",
        "main",
    )
    .await?
    .context("platform snapshot unavailable")?;
    let nodes = majnet_common::platform::NodesFile::parse(
        platform.files.get("nodes.yaml").context("no nodes.yaml")?,
    )?;
    let node = nodes
        .by_role(class.node_role())
        .context("no node for class")?;
    let docker = state.nodes(&nodes).client_for(node).await?;

    let ctx = crate::deploy::DeployCtx {
        docker: &docker,
        project,
        class,
        commit: "imperative",
        dry_run: false,
        http: &state.http,
        bot_url: &state.config.bot_url,
    };
    let restarted = crate::deploy::restart_app(&ctx, app).await?;
    anyhow::ensure!(
        restarted > 0,
        "no containers found for {project}/{app} ({})",
        class.as_str()
    );

    state.store.record(
        "imperative",
        project,
        &node.name,
        &format!("restart {app}"),
        &format!("by {actor}"),
    )?;
    tracing::info!(project, app, actor, "restarted (imperative escape hatch)");
    Ok(format!("restarted {restarted} container(s)"))
}

/// The bot's nudge — payload is informational; convergence always reconciles
/// everything from snapshots (idempotence over cleverness).
async fn notify(State(state): State<Arc<AppState>>, body: Json<serde_json::Value>) -> StatusCode {
    tracing::info!(payload = %body.0, "notified by bot");
    state.wakeup.notify_one();
    StatusCode::ACCEPTED
}

#[derive(serde::Deserialize)]
struct EventsQuery {
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    100
}

async fn events(
    State(state): State<Arc<AppState>>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Vec<crate::state::Event>>, (StatusCode, String)> {
    state
        .store
        .recent(query.limit.min(1000))
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}
