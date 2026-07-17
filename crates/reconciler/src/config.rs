//! Reconciler configuration — twelve-factor, from environment variables.

use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// Bot's WG-internal API base, e.g. `http://10.88.0.1:8081`.
    pub bot_url: String,
    /// Listener for /notify + state API. Bind to the WG IP in production.
    pub listen: String,
    /// Root platform org (platform repo lives here).
    pub root_org: String,
    /// Directory with class age keys: `age-stable.key`, `age-production.key` (§14).
    pub age_key_dir: PathBuf,
    /// Directory with Docker mTLS client material: `ca.pem`,
    /// `reconciler-cert.pem`, `reconciler-key.pem` (from bootstrap/pki).
    pub docker_cert_dir: PathBuf,
    /// SQLite event log location.
    pub data_dir: PathBuf,
    /// Drift poll interval seconds (§12.1), default 300.
    pub poll_interval_secs: u64,
    /// Log planned actions without touching Docker (§12 principles).
    pub dry_run: bool,
    /// DEV: use the local Docker socket for every node and skip the tailnet
    /// ingress. For the smoke-test harness — never in production.
    pub docker_local: bool,
    /// DEV: read snapshots from `<dir>/<org>/<repo>/<branch>/` instead of
    /// the bot. For the smoke-test harness — never in production.
    pub snapshot_dir: Option<PathBuf>,
    /// Image for the host-shell helper container (ADR 0016) — a minimal image
    /// carrying `nsenter`. Run `--privileged --pid=host` so `nsenter -t 1`
    /// enters the host namespaces. Pin by digest in production.
    pub term_helper_image: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        fn var(name: &str) -> Result<String> {
            std::env::var(name).with_context(|| format!("missing env var {name}"))
        }
        Ok(Self {
            bot_url: var("MAJNET_BOT_URL")?.trim_end_matches('/').to_string(),
            listen: std::env::var("MAJNET_LISTEN").unwrap_or_else(|_| "127.0.0.1:9090".into()),
            root_org: std::env::var("MAJNET_ROOT_ORG").unwrap_or_else(|_| "majksa-platform".into()),
            age_key_dir: std::env::var("MAJNET_AGE_KEY_DIR")
                .unwrap_or_else(|_| "/etc/majnet/age".into())
                .into(),
            docker_cert_dir: std::env::var("MAJNET_DOCKER_CERT_DIR")
                .unwrap_or_else(|_| "/etc/majnet/pki".into())
                .into(),
            data_dir: std::env::var("MAJNET_DATA_DIR")
                .unwrap_or_else(|_| "/var/lib/majnet-reconciler".into())
                .into(),
            poll_interval_secs: std::env::var("MAJNET_POLL_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
            dry_run: std::env::var("MAJNET_DRY_RUN").is_ok_and(|v| v == "1" || v == "true"),
            docker_local: std::env::var("MAJNET_DOCKER_LOCAL")
                .is_ok_and(|v| v == "1" || v == "true"),
            snapshot_dir: std::env::var("MAJNET_SNAPSHOT_DIR").ok().map(Into::into),
            term_helper_image: std::env::var("MAJNET_TERM_HELPER_IMAGE")
                .unwrap_or_else(|_| "debian:bookworm-slim".into()),
        })
    }
}
