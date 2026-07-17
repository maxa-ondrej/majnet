//! MajNet Reconciler — the single orchestrator (design doc §12).
//!
//! Consumes rendered `env/<class>` branch snapshots from the bot (it holds no
//! GitHub credentials), resolves static node placement from the environment
//! class, decrypts SOPS secrets with class keys into tmpfs, and converges
//! each node's Docker API over WireGuard (bollard, mTLS).
//!
//! Event loop: converge on bot notification OR every poll interval (~5 min
//! drift poll) — each cycle reconciles the full desired state; notifications
//! are just wakeups, never data.
//!
//! Principles (§12): idempotent; dry-run mode; every action tagged with its
//! causing commit; deletions only when config is gone from git; failed
//! decrypt/validation aborts that app loudly — no partial applies.
//!
//! Credentials held: age keys + Docker API mTLS certs. Nothing else.

mod alerts;
mod authz;
mod config;
mod converge;
mod db;
mod deploy;
mod docker;
mod gc;
mod info;
mod ingress;
mod metrics;
mod migrate;
mod platform;
mod purge;
mod rename;
mod secrets;
mod snapshot;
mod state;
mod state_api;
mod terminal;

use anyhow::Result;
use majnet_common::platform::NodesFile;
use std::sync::{Arc, OnceLock};

pub struct AppState {
    pub config: config::Config,
    pub store: state::Store,
    pub http: reqwest::Client,
    pub wakeup: tokio::sync::Notify,
    nodes: OnceLock<docker::Nodes>,
}

impl AppState {
    /// Node connection pool, initialized from the first platform snapshot.
    pub fn nodes(&self, nodes_file: &NodesFile) -> &docker::Nodes {
        self.nodes
            .get_or_init(|| docker::Nodes::new(&self.config, nodes_file))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = config::Config::from_env()?;
    let state = Arc::new(AppState {
        store: state::Store::open(&config.data_dir)?,
        http: reqwest::Client::new(),
        wakeup: tokio::sync::Notify::new(),
        nodes: OnceLock::new(),
        config,
    });

    let listener = tokio::net::TcpListener::bind(&state.config.listen).await?;
    tracing::info!(listen = %state.config.listen, dry_run = state.config.dry_run, "majnet-reconciler starting");

    let server_state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, state_api::router(server_state)).await {
            tracing::error!(error = %e, "state API server died");
        }
    });

    // Alert evaluator: metrics + site health → Discord on state transitions.
    tokio::spawn(alerts::run_loop(state.clone()));

    // The event loop (§12): converge now, then on every nudge or poll tick.
    let poll = std::time::Duration::from_secs(state.config.poll_interval_secs);
    loop {
        if let Err(e) = converge::converge_all(&state).await {
            tracing::error!(error = format!("{e:#}"), "convergence cycle failed");
        }
        tokio::select! {
            _ = state.wakeup.notified() => tracing::debug!("woken by notification"),
            _ = tokio::time::sleep(poll) => tracing::debug!("drift poll tick"),
        }
    }
}
