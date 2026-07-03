//! MajNet setup — the provisioner (ADR 0004). First run: public wizard
//! (one-time token) that creates the GitHub App, writes the bot's config,
//! seeds the platform repo, and enrolls worker nodes over SSH. After
//! /finish, only the WG-internal enrollment API remains.
//!
//! Credentials held: enrollment SSH key + PKI CA + wizard token. No GitHub,
//! no Tailscale, no age keys, no Docker client certs.

mod config;
mod enroll;
mod github_app;
mod seed;
mod state;
mod wizard;

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

pub struct AppState {
    pub config: config::Config,
    pub state: tokio::sync::Mutex<state::SetupState>,
    pub http: reqwest::Client,
    pub token: String,
    pub shutdown: tokio::sync::Notify,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = config::Config::from_env();
    let token = std::fs::read_to_string(config.token_path())
        .map(|t| t.trim().to_string())
        .with_context(|| {
            format!(
                "reading wizard token {} (install.sh generates it)",
                config.token_path().display()
            )
        })?;
    let setup_state = state::SetupState::load(&config.state_path())?;
    let done = config.done_path().exists();

    let app = Arc::new(AppState {
        state: tokio::sync::Mutex::new(setup_state),
        http: reqwest::Client::new(),
        token,
        shutdown: tokio::sync::Notify::new(),
        config,
    });

    // WG-internal: enrollment stays available for the platform's lifetime.
    let internal = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/enroll", post(wizard::enroll_handler))
        .with_state(app.clone());
    let internal_listener = tokio::net::TcpListener::bind(&app.config.listen_internal).await?;
    tracing::info!(internal = %app.config.listen_internal, done, "majnet-setup listening");

    if done {
        // Wizard already completed — never reopen the public listener.
        axum::serve(internal_listener, internal).await?;
        return Ok(());
    }

    let public = Router::new()
        .route("/", get(wizard::index))
        .route("/configure", post(wizard::configure))
        .route("/github/start", get(wizard::github_start))
        .route("/github/callback", get(wizard::github_callback))
        .route("/seed", post(wizard::seed))
        .route("/enroll", post(wizard::enroll_handler))
        .route("/finish", post(wizard::finish))
        .layer(axum::middleware::from_fn_with_state(app.clone(), auth))
        .with_state(app.clone());
    let public_listener = tokio::net::TcpListener::bind(&app.config.listen_public).await?;
    tracing::info!(public = %app.config.listen_public, "wizard is up — open /?token=<setup-token>");

    let shutdown_app = app.clone();
    tokio::try_join!(
        async {
            axum::serve(public_listener, public)
                .with_graceful_shutdown(async move { shutdown_app.shutdown.notified().await })
                .await
                .context("public wizard server")
        },
        async {
            axum::serve(internal_listener, internal)
                .await
                .context("internal server")
        },
    )?;
    Ok(())
}

/// One-time token auth: `?token=` on first visit (also echoed back by
/// GitHub's redirect), then a cookie so form posts stay authenticated.
async fn auth(
    State(app): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    let query_ok = request
        .uri()
        .query()
        .is_some_and(|q| q.split('&').any(|kv| kv == format!("token={}", app.token)));
    let cookie_ok = request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| {
            c.split(';')
                .any(|kv| kv.trim() == format!("majnet_setup={}", app.token))
        });
    if !query_ok && !cookie_ok {
        return Err((
            StatusCode::UNAUTHORIZED,
            "missing or wrong setup token — use the URL printed by install.sh".into(),
        ));
    }
    let mut response = next.run(request).await;
    if query_ok {
        response.headers_mut().insert(
            header::SET_COOKIE,
            format!("majnet_setup={}; HttpOnly; SameSite=Lax; Path=/", app.token)
                .parse()
                .expect("valid header"),
        );
    }
    Ok(response)
}
