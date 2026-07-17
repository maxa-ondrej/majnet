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
    /// Legacy raw access token; prefer an OAuth client (below), which the bot
    /// can auto-renew. Both may also be set from the dashboard Settings page,
    /// which overrides these env bootstraps (stored in the bot's `config` table).
    pub tailscale_api_key: Option<String>,
    /// Tailscale OAuth client credentials — the self-renewing alternative to a
    /// raw access token. The secret is long-lived; the bot mints short-lived
    /// API tokens from it on demand (client-credentials grant). `None` unless
    /// bootstrapped via env — the usual path is the Settings page.
    pub tailscale_oauth_client_id: Option<String>,
    pub tailscale_oauth_client_secret: Option<String>,
    /// Tailnet name, e.g. `example.com` or `tail1234.ts.net`. `-` = the
    /// authenticated identity's default tailnet.
    pub tailnet: Option<String>,
    /// Cloudflare API token (the bot's third external credential, §6 / ADR
    /// 0007): Zone→DNS→Edit + Zone→SSL and Certificates→Edit. `None` = custom
    /// domains stay a manual step (no automated DNS / origin certs).
    pub cloudflare_token: Option<String>,
    /// The `age-production` *public* recipient (ADR 0007). The bot encrypts
    /// issued origin-cert private keys to it before committing them to git;
    /// only the reconciler (holding the private key) can decrypt. `None`
    /// disables origin-cert issuance (DNS-only). Get it with
    /// `age-keygen -y /etc/majnet/age/age-production.key`.
    pub age_production_recipient: Option<String>,
    /// A GHCR credential (fine-grained/classic PAT with `read:packages`) served
    /// to the reconciler so nodes can pull **private** app images (ADR 0012).
    /// GitHub App installation tokens are not honored by GHCR for package pulls,
    /// so a PAT is required. `None` = only public images pull.
    pub ghcr_token: Option<String>,
    /// Contact email for the ACME (Let's Encrypt) account used to issue the
    /// per-project VPN ingress wildcard certs (ADR 0013). `None` disables
    /// ingress-cert issuance. Requires `cloudflare_token` (DNS-01) +
    /// `age_production_recipient` (to encrypt the key for git) as well.
    pub acme_email: Option<String>,
    /// Use Let's Encrypt's staging directory (untrusted certs, high rate limits)
    /// instead of production — for shakedown testing. `MAJNET_ACME_STAGING=1`.
    pub acme_staging: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        fn var(name: &str) -> Result<String> {
            std::env::var(name).with_context(|| format!("missing env var {name}"))
        }
        Ok(Self {
            github_app_id: var("MAJNET_GITHUB_APP_ID")?
                .parse()
                .context("MAJNET_GITHUB_APP_ID must be a number")?,
            github_private_key_path: var("MAJNET_GITHUB_PRIVATE_KEY_PATH")?.into(),
            webhook_secret: var("MAJNET_WEBHOOK_SECRET")?,
            listen_webhook: std::env::var("MAJNET_LISTEN_WEBHOOK")
                .unwrap_or_else(|_| "0.0.0.0:8080".into()),
            listen_internal: std::env::var("MAJNET_LISTEN_INTERNAL")
                .unwrap_or_else(|_| "127.0.0.1:8081".into()),
            reconciler_url: std::env::var("MAJNET_RECONCILER_URL").unwrap_or_default(),
            data_dir: std::env::var("MAJNET_DATA_DIR")
                .unwrap_or_else(|_| "/var/lib/majnet-bot".into())
                .into(),
            root_org: std::env::var("MAJNET_ROOT_ORG").unwrap_or_else(|_| "majksa-platform".into()),
            tailscale_api_key: std::env::var("MAJNET_TAILSCALE_API_KEY")
                .ok()
                .filter(|v| !v.is_empty()),
            tailscale_oauth_client_id: std::env::var("MAJNET_TAILSCALE_OAUTH_CLIENT_ID")
                .ok()
                .filter(|v| !v.is_empty()),
            tailscale_oauth_client_secret: std::env::var("MAJNET_TAILSCALE_OAUTH_CLIENT_SECRET")
                .ok()
                .filter(|v| !v.is_empty()),
            tailnet: std::env::var("MAJNET_TAILNET")
                .ok()
                .filter(|v| !v.is_empty()),
            cloudflare_token: std::env::var("MAJNET_CLOUDFLARE_TOKEN")
                .ok()
                .filter(|v| !v.is_empty()),
            age_production_recipient: std::env::var("MAJNET_AGE_PRODUCTION_RECIPIENT")
                .ok()
                .filter(|v| !v.is_empty()),
            ghcr_token: std::env::var("MAJNET_GHCR_TOKEN")
                .ok()
                .filter(|v| !v.is_empty()),
            acme_email: std::env::var("MAJNET_ACME_EMAIL")
                .ok()
                .filter(|v| !v.is_empty()),
            acme_staging: std::env::var("MAJNET_ACME_STAGING")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        })
    }
}
