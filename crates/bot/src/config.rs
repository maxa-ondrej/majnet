//! Bot configuration — twelve-factor, from environment variables.

use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// GitHub App ID.
    pub github_app_id: u64,
    /// Path to the GitHub App private key (PEM). The one GitHub credential (§6).
    pub github_private_key_path: PathBuf,
    /// Webhook HMAC secret shared with the GitHub App configuration.
    pub webhook_secret: String,
    /// Public listener (webhooks from GitHub, proxied via edge). E.g. `0.0.0.0:8080`.
    pub listen_webhook: String,
    /// WG-internal listener (snapshot API for the reconciler). MUST be bound
    /// to the main node's WireGuard IP, e.g. `10.88.0.1:8081`.
    pub listen_internal: String,
    /// Reconciler notify endpoint over WG, e.g. `http://10.88.0.1:9090`.
    /// Empty = notifications disabled (reconciler not deployed yet).
    pub reconciler_url: String,
    /// Data directory (SQLite DB, snapshot cache).
    pub data_dir: PathBuf,
    /// Root platform org (registry + platform config live here) — §2.
    pub root_org: String,
    /// Tailscale API key (the bot's second credential, §6). Empty = TS sync off.
    pub tailscale_api_key: Option<String>,
    /// Tailnet name, e.g. `example.com` or `tail1234.ts.net`.
    pub tailnet: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        fn var(name: &str) -> Result<String> {
            std::env::var(name).with_context(|| format!("missing env var {name}"))
        }
        Ok(Self {
            github_app_id: var("MAJNET_GITHUB_APP_ID")?.parse().context("MAJNET_GITHUB_APP_ID must be a number")?,
            github_private_key_path: var("MAJNET_GITHUB_PRIVATE_KEY_PATH")?.into(),
            webhook_secret: var("MAJNET_WEBHOOK_SECRET")?,
            listen_webhook: std::env::var("MAJNET_LISTEN_WEBHOOK").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            listen_internal: std::env::var("MAJNET_LISTEN_INTERNAL").unwrap_or_else(|_| "127.0.0.1:8081".into()),
            reconciler_url: std::env::var("MAJNET_RECONCILER_URL").unwrap_or_default(),
            data_dir: std::env::var("MAJNET_DATA_DIR").unwrap_or_else(|_| "/var/lib/majnet-bot".into()).into(),
            root_org: std::env::var("MAJNET_ROOT_ORG").unwrap_or_else(|_| "majksa-platform".into()),
            tailscale_api_key: std::env::var("MAJNET_TAILSCALE_API_KEY").ok(),
            tailnet: std::env::var("MAJNET_TAILNET").ok(),
        })
    }
}
