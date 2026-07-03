//! Node enrollment over SSH (ADR 0004): push the `bootstrap/` payload,
//! render node.env, distribute PKI server certs, run bootstrap remotely,
//! capture the WG pubkey, re-render peers everywhere, and register the node
//! in nodes.yaml via the bot. The bootstrap scripts stay the real payload —
//! this module only executes them.
//!
//! SSH users: first contact is `root` (fresh Debian); bootstrap's 10-base
//! then disables root login, so everything after `bootstrap.sh` goes through
//! the `majnet` admin user (its authorized_keys include the enrollment key)
//! with sudo. Pre-bootstrap steps fall back to admin+sudo so re-running an
//! enrollment on an already-hardened node still works.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fmt::Write as _;

use crate::config::Config;
use crate::state::{wireguard_ip, NodeEntry, SetupState};

/// Must match ADMIN_USER in the generated node.env.
const ADMIN_USER: &str = "majnet";

#[derive(Debug, Deserialize)]
pub struct EnrollRequest {
    /// Node name — by convention the role name (single node per role).
    pub name: String,
    /// `prod` | `private` (main enrolls itself at install).
    pub role: String,
    /// SSH destination: the node's public IP or hostname, with the
    /// enrollment pubkey authorized for root (fresh) or majnet (re-run).
    pub ssh_host: String,
}

/// Run the whole enrollment; returns the step-by-step log shown to the
/// operator. The caller persists state afterwards.
pub async fn run(
    config: &Config,
    state: &mut SetupState,
    http: &reqwest::Client,
    req: &EnrollRequest,
) -> Result<String> {
    validate(req)?;
    let wg_ip = wireguard_ip(&req.role).context("role has no static WG IP")?;
    let mut log = String::new();

    ensure_main_registered(config, state).await?;
    let enroll_key = enroll_pubkey(config);

    writeln!(
        log,
        "→ issuing PKI material (CA at {})",
        config.pki_dir().display()
    )
    .ok();
    gen_certs(config).await?;

    writeln!(log, "→ pushing bootstrap payload to {}", req.ssh_host).ok();
    push_payload(config, &req.ssh_host).await?;

    writeln!(log, "→ writing node.env + PKI server certs").ok();
    let node_env = render_node_env(&req.name, &req.role, wg_ip, state, &enroll_key);
    write_file(config, &req.ssh_host, "/etc/majnet/node.env", &node_env).await?;
    for (local, remote) in [
        ("ca.pem", "ca.pem"),
        (&format!("server-{}-cert.pem", req.role), "server-cert.pem"),
        (&format!("server-{}-key.pem", req.role), "server-key.pem"),
    ] {
        let content = std::fs::read_to_string(config.pki_dir().join(local))
            .with_context(|| format!("reading PKI file {local}"))?;
        write_file(
            config,
            &req.ssh_host,
            &format!("/etc/majnet/pki/{remote}"),
            &content,
        )
        .await?;
    }

    writeln!(
        log,
        "→ running bootstrap.sh on {} (this takes a while)",
        req.ssh_host
    )
    .ok();
    let out = exec(
        config,
        &req.ssh_host,
        "bash /opt/majnet/bootstrap/bootstrap.sh",
        900,
    )
    .await?;
    log.push_str(&indent(&out));

    writeln!(log, "→ collecting WireGuard pubkey").ok();
    // Root is disabled by now — admin + sudo from here on.
    let pubkey = ssh(
        config,
        ADMIN_USER,
        &req.ssh_host,
        "sudo sh -c 'wg pubkey < /etc/wireguard/wg0.key'",
        "",
        30,
    )
    .await?
    .trim()
    .to_string();
    anyhow::ensure!(!pubkey.is_empty(), "node returned an empty WG pubkey");

    let entry = NodeEntry {
        role: req.role.clone(),
        ssh_host: req.ssh_host.clone(),
        wireguard_ip: wg_ip.into(),
        public_endpoint: format!("{}:51820", req.ssh_host),
        wireguard_pubkey: pubkey,
    };
    state.nodes.insert(req.name.clone(), entry.clone());

    writeln!(log, "→ re-rendering WireGuard peers on all nodes").ok();
    for (name, node) in state.nodes.clone() {
        if name == req.name {
            continue;
        }
        let env = render_node_env(&name, &node.role, &node.wireguard_ip, state, &enroll_key);
        // Steps 10+20: 10 propagates admin-key changes (wizard-collected
        // keys reach nodes only through authorized_keys re-renders), 20 the
        // new peer.
        let out = if node.ssh_host.is_empty() {
            // The local node (main).
            std::fs::write(config.etc_dir.join("node.env"), &env)?;
            local_sh(
                &format!(
                    "bash {}/bootstrap/bootstrap.sh 10 20",
                    config.repo_dir.display()
                ),
                "",
                300,
            )
            .await?
        } else {
            ssh(
                config,
                ADMIN_USER,
                &node.ssh_host,
                "sudo install -D -m 0600 /dev/stdin /etc/majnet/node.env && sudo bash /opt/majnet/bootstrap/bootstrap.sh 10 20",
                &env,
                300,
            )
            .await?
        };
        log.push_str(&indent(&out));
    }

    writeln!(log, "→ registering in nodes.yaml (via the bot)").ok();
    register_node(config, http, &req.name, &entry).await?;

    writeln!(
        log,
        "✓ {} enrolled ({} @ {})",
        req.name, req.role, entry.wireguard_ip
    )
    .ok();
    Ok(log)
}

/// Main registers itself from local material (installer ran bootstrap here).
pub async fn ensure_main_registered(config: &Config, state: &mut SetupState) -> Result<()> {
    if state.nodes.contains_key("main") {
        return Ok(());
    }
    let pubkey = local_sh("wg pubkey < /etc/wireguard/wg0.key", "", 15)
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let endpoint = if state.public_host.is_empty() {
        String::new()
    } else {
        format!("{}:51820", state.public_host)
    };
    state.nodes.insert(
        "main".into(),
        NodeEntry {
            role: "main".into(),
            ssh_host: String::new(),
            wireguard_ip: wireguard_ip("main").expect("static").into(),
            public_endpoint: endpoint,
            wireguard_pubkey: pubkey,
        },
    );
    state.save(&config.state_path())?;
    Ok(())
}

pub async fn register_node(
    config: &Config,
    http: &reqwest::Client,
    name: &str,
    entry: &NodeEntry,
) -> Result<()> {
    let node = majnet_common::platform::Node {
        name: name.into(),
        role: entry.role.clone(),
        wireguard_ip: entry.wireguard_ip.clone(),
        public_endpoint: entry.public_endpoint.clone(),
        wireguard_pubkey: entry.wireguard_pubkey.clone(),
    };
    let resp = http
        .post(format!("{}/api/platform/node", config.bot_url))
        .json(&node)
        .send()
        .await
        .context("reaching the bot")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    anyhow::ensure!(status.is_success(), "bot rejected node ({status}): {body}");
    Ok(())
}

fn validate(req: &EnrollRequest) -> Result<()> {
    anyhow::ensure!(
        matches!(req.role.as_str(), "prod" | "private"),
        "role must be prod|private (main enrolls itself at install)"
    );
    anyhow::ensure!(
        !req.name.is_empty()
            && req
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "invalid node name"
    );
    anyhow::ensure!(
        !req.ssh_host.is_empty()
            && req
                .ssh_host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':')),
        "invalid ssh host"
    );
    Ok(())
}

/// node.env for any node, from wizard state — the single source for the
/// whole mesh (peers = every *other* known node with a pubkey). The
/// enrollment pubkey is always an admin key: post-bootstrap SSH depends on it.
fn render_node_env(
    name: &str,
    role: &str,
    wg_ip: &str,
    state: &SetupState,
    enroll_key: &str,
) -> String {
    let mut peers = String::new();
    for (peer_name, peer) in &state.nodes {
        if peer_name == name || peer.wireguard_pubkey.is_empty() {
            continue;
        }
        // 20-wireguard.sh parses name:ip:pubkey:host:port; public_endpoint
        // is already host:port.
        writeln!(
            peers,
            "{peer_name}:{}:{}:{}",
            peer.wireguard_ip, peer.wireguard_pubkey, peer.public_endpoint
        )
        .ok();
    }
    let admin_keys = format!("{}\n{}", state.admin_ssh_keys.trim(), enroll_key.trim());
    format!(
        "# Generated by majnet-setup (ADR 0004) — re-rendered on enrollment.\n\
         NODE_NAME={name}\n\
         NODE_ROLE={role}\n\
         WG_ADDRESS={wg_ip}/24\n\
         WG_LISTEN_PORT=51820\n\
         WG_PEERS=\"\n{peers}\"\n\
         DOCKER_API_PORT=2376\n\
         ADMIN_USER={ADMIN_USER}\n\
         ADMIN_SSH_KEYS=\"\n{}\n\"\n\
         BESZEL_HUB_URL=http://10.88.0.1:8090\n\
         BESZEL_AGENT_KEY=\n\
         RESTIC_REPOSITORY=\n",
        admin_keys.trim()
    )
}

/// Re-run gen-certs.sh — idempotent (skips existing certs, keeps the CA).
async fn gen_certs(config: &Config) -> Result<()> {
    let out = local_sh(
        &format!(
            "bash '{}' '{}'",
            config.repo_dir.join("bootstrap/pki/gen-certs.sh").display(),
            config.pki_dir().display()
        ),
        "",
        60,
    )
    .await?;
    tracing::debug!(out, "gen-certs");
    Ok(())
}

/// The payload travels base64-encoded on stdin — keeps the transfer on the
/// same argv-based ssh path as everything else (no local shell pipelines).
async fn push_payload(config: &Config, host: &str) -> Result<()> {
    let payload = local_sh(
        &format!(
            "tar -C '{}' -cz bootstrap | base64",
            config.repo_dir.display()
        ),
        "",
        60,
    )
    .await?;
    exec_with_stdin(
        config,
        host,
        "mkdir -p /opt/majnet /etc/majnet/pki && base64 -d | tar -C /opt/majnet -xz",
        &payload,
        600,
    )
    .await
    .context("pushing bootstrap payload")?;
    Ok(())
}

/// Write a root-owned 0600 file on the node.
async fn write_file(config: &Config, host: &str, path: &str, content: &str) -> Result<()> {
    exec_with_stdin(
        config,
        host,
        &format!("install -D -m 0600 /dev/stdin {path}"),
        content,
        60,
    )
    .await
    .with_context(|| format!("writing {path} on {host}"))?;
    Ok(())
}

/// Run a command as root, falling back to admin+sudo (hardened node).
async fn exec(config: &Config, host: &str, cmd: &str, timeout: u64) -> Result<String> {
    exec_with_stdin(config, host, cmd, "", timeout).await
}

async fn exec_with_stdin(
    config: &Config,
    host: &str,
    cmd: &str,
    stdin: &str,
    timeout: u64,
) -> Result<String> {
    match ssh(config, "root", host, cmd, stdin, timeout).await {
        Ok(out) => Ok(out),
        Err(root_err) => ssh(
            config,
            ADMIN_USER,
            host,
            &format!("sudo {cmd}"),
            stdin,
            timeout,
        )
        .await
        .with_context(|| format!("as root: {root_err:#}")),
    }
}

/// One SSH invocation — argv-based, no local shell, remote command passed as
/// a single argument.
async fn ssh(
    config: &Config,
    user: &str,
    host: &str,
    remote_cmd: &str,
    stdin: &str,
    timeout: u64,
) -> Result<String> {
    let key = config.ssh_key_path().display().to_string();
    run_cmd(
        "ssh",
        &[
            "-i",
            &key,
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "ConnectTimeout=15",
            &format!("{user}@{host}"),
            remote_cmd,
        ],
        stdin,
        timeout,
    )
    .await
    .with_context(|| format!("ssh {user}@{host}: {remote_cmd}"))
}

async fn local_sh(cmd: &str, stdin: &str, timeout: u64) -> Result<String> {
    run_cmd("sh", &["-c", cmd], stdin, timeout).await
}

async fn run_cmd(program: &str, args: &[&str], stdin: &str, timeout_secs: u64) -> Result<String> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {program}"))?;
    if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(stdin.as_bytes()).await?;
        drop(pipe);
    }
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait_with_output(),
    )
    .await
    .with_context(|| format!("timed out after {timeout_secs}s: {program}"))??;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    anyhow::ensure!(
        out.status.success(),
        "command failed ({}): {}",
        out.status,
        combined.trim()
    );
    Ok(combined)
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("    {l}\n")).collect()
}

/// The enrollment pubkey operators must authorize on new nodes.
pub fn enroll_pubkey(config: &Config) -> String {
    std::fs::read_to_string(config.ssh_key_path().with_extension("pub"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "(enrollment key not generated yet — run install.sh)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_main() -> SetupState {
        let mut state = SetupState {
            admin_ssh_keys: "ssh-ed25519 AAAA... op".into(),
            ..Default::default()
        };
        state.nodes.insert(
            "main".into(),
            NodeEntry {
                role: "main".into(),
                ssh_host: String::new(),
                wireguard_ip: "10.88.0.1".into(),
                public_endpoint: "203.0.113.1:51820".into(),
                wireguard_pubkey: "MAINPUB".into(),
            },
        );
        state
    }

    #[test]
    fn node_env_lists_other_peers_in_bootstrap_format() {
        let env = render_node_env(
            "prod",
            "prod",
            "10.88.0.2",
            &state_with_main(),
            "ssh-ed25519 ENROLL setup",
        );
        assert!(env.contains("NODE_ROLE=prod"));
        assert!(env.contains("WG_ADDRESS=10.88.0.2/24"));
        // name:ip:pubkey:host:port — what 20-wireguard.sh parses.
        assert!(env.contains("main:10.88.0.1:MAINPUB:203.0.113.1:51820"));
        assert!(env.contains("ssh-ed25519 AAAA... op"));
        // Post-bootstrap SSH depends on the enrollment key being an admin key.
        assert!(env.contains("ssh-ed25519 ENROLL setup"));
    }

    #[test]
    fn node_env_skips_self_and_pubkeyless_peers() {
        let mut state = state_with_main();
        state.nodes.insert(
            "prod".into(),
            NodeEntry {
                role: "prod".into(),
                ssh_host: "198.51.100.2".into(),
                wireguard_ip: "10.88.0.2".into(),
                public_endpoint: "198.51.100.2:51820".into(),
                wireguard_pubkey: String::new(), // not bootstrapped yet
            },
        );
        let env = render_node_env("main", "main", "10.88.0.1", &state, "");
        assert!(!env.contains("main:10.88.0.1:")); // not its own peer
        assert!(!env.contains("prod:10.88.0.2:")); // no pubkey yet
    }

    #[test]
    fn rejects_bad_input() {
        for (name, role, host) in [
            ("prod", "main", "1.2.3.4"),        // can't enroll main
            ("Prod", "prod", "1.2.3.4"),        // uppercase name
            ("prod", "prod", "host; rm -rf /"), // shell metacharacters
        ] {
            assert!(validate(&EnrollRequest {
                name: name.into(),
                role: role.into(),
                ssh_host: host.into(),
            })
            .is_err());
        }
    }
}
