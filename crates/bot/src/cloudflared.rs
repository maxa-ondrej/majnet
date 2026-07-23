//! WG-internal endpoint: provision a Cloudflare Tunnel for a project's public
//! non-production apps and hand the reconciler the tunnel token (ADR 0026).
//!
//! Mirrors `tailscale::authkey`: the reconciler calls this when it (re)creates a
//! project's ingress and any app has `ingress.public: true`. Trust comes from the
//! bind address (WG-internal listener), like every other reconciler→bot call.
//! Credential isolation: only the bot touches the Cloudflare API; the reconciler
//! receives just the scoped tunnel token.

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

use crate::cloudflare::Cloudflare;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct TunnelRequest {
    /// The public hostnames to route to this project's Traefik (the `ingress.host`
    /// of each app with `ingress.public: true`). The reconciler already parses the
    /// manifests, so it passes the hosts here — the bot needn't read the ops repo.
    pub hosts: Vec<String>,
}

/// `POST /api/cloudflare-tunnel/{project}` — provision the tunnel + proxied DNS,
/// return the tunnel token as the bare body.
pub async fn token(
    State(state): State<Arc<AppState>>,
    Path(project): Path<String>,
    Json(req): Json<TunnelRequest>,
) -> Result<String, (StatusCode, String)> {
    mint(&state, &project, &req.hosts)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn mint(state: &AppState, project: &str, hosts: &[String]) -> Result<String> {
    anyhow::ensure!(
        project
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "invalid project name"
    );
    let cf_token = state
        .config
        .cloudflare_token
        .clone()
        .context("Cloudflare API token not configured on the bot")?;
    let cf = Cloudflare::new(state.http.clone(), cf_token);
    let token = cf
        .provision_tunnel(&format!("majnet-{project}"), hosts)
        .await
        .context("provisioning Cloudflare tunnel")?;
    state.store.log_event("cf-tunnel", None, project)?;
    Ok(token)
}
