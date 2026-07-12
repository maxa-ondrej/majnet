//! Wizard progress, persisted to `<etc>/setup-state.json` (0600 — it holds
//! the Tailscale API key until it lands in bot.env; same protection class).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SetupState {
    #[serde(default)]
    pub root_org: String,
    #[serde(default)]
    pub tailnet: String,
    #[serde(default)]
    pub tailscale_api_key: String,
    /// GHCR pull token (classic PAT, `read:packages`) → `MAJNET_GHCR_TOKEN` in
    /// bot.env, so nodes can pull private app images (ADR 0012).
    #[serde(default)]
    pub ghcr_token: String,
    /// Public IP/hostname of the main node (webhook + wizard callbacks).
    #[serde(default)]
    pub public_host: String,
    /// Operator SSH pubkeys, newline-separated (→ node.env ADMIN_SSH_KEYS).
    #[serde(default)]
    pub admin_ssh_keys: String,
    #[serde(default)]
    pub app_id: Option<u64>,
    #[serde(default)]
    pub app_slug: Option<String>,
    #[serde(default)]
    pub seeded: bool,
    /// Enrolled nodes by name (main registers itself, workers via SSH).
    #[serde(default)]
    pub nodes: BTreeMap<String, NodeEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeEntry {
    pub role: String,
    /// SSH destination; empty = the local node (main).
    #[serde(default)]
    pub ssh_host: String,
    pub wireguard_ip: String,
    #[serde(default)]
    pub public_endpoint: String,
    #[serde(default)]
    pub wireguard_pubkey: String,
}

impl SetupState {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn configured(&self) -> bool {
        !self.root_org.is_empty() && !self.public_host.is_empty()
    }
}

/// Static WG addressing (bootstrap convention: main=.1, prod=.2, private=.3).
pub fn wireguard_ip(role: &str) -> Option<&'static str> {
    match role {
        "main" => Some("10.88.0.1"),
        "prod" => Some("10.88.0.2"),
        "private" => Some("10.88.0.3"),
        _ => None,
    }
}
