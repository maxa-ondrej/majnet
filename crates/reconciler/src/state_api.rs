//! WG-internal API (§12.8): the bot's deploy nudge + read-only state for the
//! dashboard. The phase-5 restart escape hatch (§16) will live here too.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use std::sync::Arc;

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
