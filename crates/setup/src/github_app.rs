//! GitHub App creation via the manifest flow (ADR 0004): we POST a manifest
//! form to GitHub, the operator confirms, GitHub redirects back with a
//! one-time `code`, and the conversion exchange returns the App credentials
//! — which go straight into the bot's config, never into wizard state.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

use crate::config::Config;
use crate::state::SetupState;

/// The manifest posted to GitHub. Permissions/events must match what the bot
/// handles (`crates/bot/README.md`): contents/PRs/administration/members RW,
/// packages R; push, pull_request, registry_package.
pub fn manifest(state: &SetupState, public_base: Option<&str>, nonce: &str) -> serde_json::Value {
    // Behind Caddy (ADR 0006) both endpoints share one https host, path-routed;
    // without it they are the raw per-service ports.
    let (hook_url, redirect_url) = match public_base {
        Some(base) => (
            format!("{base}/webhook"),
            format!("{base}/github/callback?token={nonce}"),
        ),
        None => (
            format!("http://{}:8080/webhook", state.public_host),
            format!(
                "http://{}:7600/github/callback?token={nonce}",
                state.public_host
            ),
        ),
    };
    json!({
        "name": format!("majnet-{}", state.root_org),
        "url": format!("https://github.com/{}", state.root_org),
        "hook_attributes": { "url": hook_url },
        "redirect_url": redirect_url,
        // Public so it can be installed on each project org (§2); a private
        // App installs only on its owning org. The projects.yaml registry —
        // not installability — is the discovery gate, so a stray install is
        // inert.
        "public": true,
        "default_permissions": {
            "contents": "write",
            "pull_requests": "write",
            "administration": "write",
            "members": "write",
            "packages": "read",
        },
        "default_events": ["push", "pull_request", "registry_package", "release"],
    })
}

/// Where the manifest form is submitted (org-scoped App).
pub fn submit_url(state: &SetupState) -> String {
    format!(
        "https://github.com/organizations/{}/settings/apps/new",
        state.root_org
    )
}

#[derive(Debug, Deserialize)]
pub struct AppCredentials {
    pub id: u64,
    pub slug: String,
    pub pem: String,
    pub webhook_secret: String,
    pub html_url: String,
}

/// `POST /app-manifests/{code}/conversions` — no auth required; the code is
/// single-use and expires in one hour.
pub async fn exchange(http: &reqwest::Client, code: &str) -> Result<AppCredentials> {
    let resp = http
        .post(format!(
            "https://api.github.com/app-manifests/{code}/conversions"
        ))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "majnet-setup")
        .send()
        .await
        .context("reaching api.github.com")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "manifest conversion failed ({status}): {body}"
    );
    serde_json::from_str(&body).context("parsing conversion response")
}

/// Write the bot's whole config: PEM + bot.env. Values already present in
/// bot.env are preserved unless overridden, so /configure can run before or
/// after the App exists.
pub fn write_bot_config(
    config: &Config,
    state: &SetupState,
    creds: Option<&AppCredentials>,
) -> Result<()> {
    let mut env = read_env_file(&config.bot_env_path());

    if let Some(creds) = creds {
        std::fs::write(config.app_pem_path(), &creds.pem)
            .with_context(|| format!("writing {}", config.app_pem_path().display()))?;
        restrict(&config.app_pem_path())?;
        env.insert("MAJNET_GITHUB_APP_ID".into(), creds.id.to_string());
        env.insert(
            "MAJNET_GITHUB_PRIVATE_KEY_PATH".into(),
            config.app_pem_path().display().to_string(),
        );
        env.insert("MAJNET_WEBHOOK_SECRET".into(), creds.webhook_secret.clone());
    }
    env.insert("MAJNET_ROOT_ORG".into(), state.root_org.clone());
    env.insert("MAJNET_LISTEN_WEBHOOK".into(), "0.0.0.0:8080".into());
    env.insert("MAJNET_LISTEN_INTERNAL".into(), "10.88.0.1:8081".into());
    env.insert(
        "MAJNET_RECONCILER_URL".into(),
        "http://10.88.0.1:9090".into(),
    );
    if !state.tailscale_api_key.is_empty() {
        env.insert(
            "MAJNET_TAILSCALE_API_KEY".into(),
            state.tailscale_api_key.clone(),
        );
    }
    if !state.tailnet.is_empty() {
        env.insert("MAJNET_TAILNET".into(), state.tailnet.clone());
    }

    let content: String = env.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    std::fs::write(config.bot_env_path(), content)
        .with_context(|| format!("writing {}", config.bot_env_path().display()))?;
    restrict(&config.bot_env_path())?;
    Ok(())
}

/// Best-effort bring-up of the bot after its App credentials were written
/// (ADR 0008: it runs as a compose service, so `up -d bot` — not systemctl).
/// On a fresh install this is what first starts the bot; on a re-config it
/// recreates it. Absent outside a node.
pub async fn restart_bot() {
    match tokio::process::Command::new("docker")
        .args([
            "compose",
            "-f",
            "/opt/majnet/deploy/compose.yaml",
            "up",
            "-d",
            "--force-recreate",
            "bot",
        ])
        .output()
        .await
    {
        Ok(out) if out.status.success() => tracing::info!("majnet-bot (re)started via compose"),
        Ok(out) => tracing::warn!(
            stderr = String::from_utf8_lossy(&out.stderr).trim(),
            "docker compose up bot failed"
        ),
        Err(e) => tracing::warn!(error = %e, "docker unavailable — start the bot manually"),
    }
}

fn read_env_file(path: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

fn restrict(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_matches_bot_readme() {
        let state = SetupState {
            root_org: "majksa-platform".into(),
            public_host: "203.0.113.1".into(),
            ..Default::default()
        };
        let m = manifest(&state, None, "tok");
        assert_eq!(m["default_permissions"]["contents"], "write");
        assert_eq!(m["default_permissions"]["packages"], "read");
        assert!(m["default_events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == "registry_package"));
        assert_eq!(
            m["hook_attributes"]["url"],
            "http://203.0.113.1:8080/webhook"
        );
        assert_eq!(
            submit_url(&state),
            "https://github.com/organizations/majksa-platform/settings/apps/new"
        );

        // Behind Caddy (ADR 0006): one https host, path-routed.
        let m = manifest(&state, Some("https://majnet.example.com"), "tok");
        assert_eq!(
            m["hook_attributes"]["url"],
            "https://majnet.example.com/webhook"
        );
        assert_eq!(
            m["redirect_url"],
            "https://majnet.example.com/github/callback?token=tok"
        );
    }
}
