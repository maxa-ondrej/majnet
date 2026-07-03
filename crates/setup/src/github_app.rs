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
pub fn manifest(state: &SetupState, nonce: &str) -> serde_json::Value {
    json!({
        "name": format!("majnet-{}", state.root_org),
        "url": format!("https://github.com/{}", state.root_org),
        "hook_attributes": { "url": format!("http://{}:8080/webhook", state.public_host) },
        "redirect_url": format!("http://{}:7600/github/callback?token={nonce}", state.public_host),
        "public": false,
        "default_permissions": {
            "contents": "write",
            "pull_requests": "write",
            "administration": "write",
            "members": "write",
            "packages": "read",
        },
        "default_events": ["push", "pull_request", "registry_package"],
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

/// Best-effort `systemctl restart majnet-bot` — absent outside a node.
pub async fn restart_bot() {
    match tokio::process::Command::new("systemctl")
        .args(["restart", "majnet-bot"])
        .output()
        .await
    {
        Ok(out) if out.status.success() => tracing::info!("majnet-bot restarted"),
        Ok(out) => tracing::warn!(
            stderr = String::from_utf8_lossy(&out.stderr).trim(),
            "systemctl restart majnet-bot failed"
        ),
        Err(e) => tracing::warn!(error = %e, "systemctl unavailable — restart the bot manually"),
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
        let m = manifest(&state, "tok");
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
    }
}
