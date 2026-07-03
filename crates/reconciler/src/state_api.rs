//! WG-internal API (§12.8): the bot's deploy nudge + read-only state for the
//! dashboard. The phase-5 restart escape hatch (§16) will live here too.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use std::sync::Arc;

use crate::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/notify", post(notify))
        .route("/api/events", get(events))
        .route("/api/restart/{project}/{class}/{app}", post(restart))
        .with_state(state)
}

/// The one imperative escape hatch (§16): restart isn't a state change, so it
/// can't be a commit. Audit-logged with the acting identity (Tailscale serve
/// injects `Tailscale-User-Login` when fronted by it). Nothing else is
/// imperative.
async fn restart(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((project, class, app)): axum::extract::Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<String, (StatusCode, String)> {
    let actor = headers
        .get("tailscale-user-login")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    do_restart(&state, &project, &class, &app, &actor)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn do_restart(
    state: &AppState,
    project: &str,
    class: &str,
    app: &str,
    actor: &str,
) -> anyhow::Result<String> {
    use anyhow::Context;
    let class: majnet_common::EnvClass = serde_yaml::from_str(class)
        .map_err(|_| anyhow::anyhow!("class must be production|stable|ephemeral"))?;

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
