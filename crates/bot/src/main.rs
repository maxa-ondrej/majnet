//! MajNet GitHub Bot — the only component talking to GitHub and Tailscale APIs.
//!
//! Responsibilities (design doc §11); phase 1 implements 1, 3, 4, 8:
//!  1. GitHub App auth (JWT → per-org installation tokens)        [github]
//!  3. Webhook intake across all project orgs                     [webhooks]
//!  4. Digest bumps — App-signed commits to project `ops` repos   [digest]
//!  8. Repo access proxy — snapshots for the reconciler over WG   [proxy, notify]
//!
//! Phase 2: manifest rendering [render]. Phase 3: org reconciliation
//! [org_sync] + Tailscale sync [tailscale]. Phase 5: dashboard write API.
//!
//! Credentials held: GitHub App key + Tailscale API key. Nothing else.

mod acme;
mod authz;
mod cloudflare;
mod config;
mod dashboard_api;
mod deploys;
mod digest;
mod ephemeral;
mod git;
mod github;
mod migrate;
mod notify;
mod org_sync;
mod platform_api;
mod promote;
mod proxy;
mod registry;
mod releases;
mod render;
mod state;
mod tailscale;
mod webhooks;

use anyhow::{Context, Result};
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

pub struct AppState {
    pub config: config::Config,
    pub github: github::GitHub,
    pub store: state::Store,
    pub http: reqwest::Client,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = config::Config::from_env()?;
    let key = std::fs::read(&config.github_private_key_path)
        .with_context(|| format!("reading {}", config.github_private_key_path.display()))?;
    let state = Arc::new(AppState {
        github: github::GitHub::new(config.github_app_id, &key)?,
        store: state::Store::open(&config.data_dir)?,
        http: reqwest::Client::new(),
        config,
    });

    // Public listener: GitHub webhooks (reaches us via edge / tunnel).
    let webhook_app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/webhook", post(webhooks::handle))
        .with_state(state.clone());

    // WG-internal listener: the reconciler's snapshot + authkey API. Trust
    // comes from the bind address being the WireGuard IP (§7) — keep it so.
    let internal_app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/snapshot/{org}/{repo}/{branch}", get(proxy::snapshot))
        .route("/api/registry-auth/{org}", get(proxy::registry_auth))
        .route("/api/tailscale-authkey/{project}", post(tailscale::authkey))
        .route("/api/promote/{org}/{app}", post(promote::promote))
        .route("/api/rollback/{org}", post(promote::rollback))
        .route("/api/platform/seed", post(platform_api::seed))
        .route("/api/platform/node", post(platform_api::upsert_node))
        .route("/api/platform/version", get(platform_api::version))
        .route(
            "/api/manifest/{org}/{app}",
            get(dashboard_api::manifest_get),
        )
        .route(
            "/api/manifest/{org}/{app}/{file}",
            axum::routing::put(dashboard_api::manifest_put),
        )
        .route(
            "/api/members/{org}",
            get(dashboard_api::members_get).post(dashboard_api::members_post),
        )
        .route("/api/whoami", get(dashboard_api::whoami))
        .route(
            "/api/projects",
            get(dashboard_api::projects_get).post(dashboard_api::projects_post),
        )
        .route(
            "/api/apps/{org}",
            get(dashboard_api::apps_get).post(dashboard_api::apps_post),
        )
        .route(
            "/api/apps/{org}/{app}/rename",
            post(dashboard_api::app_rename_post),
        )
        .route(
            "/api/projects/{org}/rename",
            post(dashboard_api::project_rename_post),
        )
        .route(
            "/api/apps/{org}/{app}/archive",
            post(dashboard_api::app_archive_post),
        )
        .route(
            "/api/apps/{org}/{app}/delete",
            post(dashboard_api::app_delete_post),
        )
        .route("/api/archived/{org}", get(dashboard_api::archived_get))
        .route(
            "/api/secrets/{org}/{app}",
            post(dashboard_api::secrets_post),
        )
        .route("/api/nodes", get(dashboard_api::nodes_get))
        .route("/api/imports/{org}", get(dashboard_api::imports_get))
        .route(
            "/api/platform/registry",
            get(dashboard_api::registry_status).post(dashboard_api::registry_set),
        )
        .route(
            "/api/imports/{org}/{app}/retry",
            post(dashboard_api::imports_retry),
        )
        .route("/api/releases/{org}/{app}", get(releases::list))
        .route(
            "/api/releases/{org}/{app}/backfill",
            post(releases::backfill_post),
        )
        .route(
            "/api/releases/{org}/{app}/promote/{version}",
            post(releases::promote),
        )
        .route("/api/deploys/{org}", get(deploys::list))
        .route("/api/deploys/{org}/{number}/merge", post(deploys::merge))
        .route("/api/deploys/{org}/{number}/close", post(deploys::close))
        .with_state(state.clone());

    // Org reconciliation: hourly, plus webhook-triggered on config pushes (§11.2).
    let sync_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            if let Err(e) = org_sync::sync_all(&sync_state).await {
                tracing::error!(error = format!("{e:#}"), "scheduled org sync failed");
            }
        }
    });

    let webhook_listener = tokio::net::TcpListener::bind(&state.config.listen_webhook).await?;
    let internal_listener = tokio::net::TcpListener::bind(&state.config.listen_internal).await?;
    tracing::info!(
        webhook = %state.config.listen_webhook,
        internal = %state.config.listen_internal,
        "majnet-bot listening"
    );

    tokio::try_join!(
        async {
            axum::serve(webhook_listener, webhook_app)
                .await
                .context("webhook server")
        },
        async {
            axum::serve(internal_listener, internal_app)
                .await
                .context("internal server")
        },
    )?;
    Ok(())
}
