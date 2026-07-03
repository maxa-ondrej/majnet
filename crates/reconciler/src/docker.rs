//! Docker connections — bollard over WireGuard with mTLS (§7). One client
//! per node, keyed by role; certs from bootstrap/pki (the reconciler's only
//! infrastructure credential besides age keys).

use anyhow::{Context, Result};
use bollard::Docker;
use majnet_common::platform::{Node, NodesFile};
use std::collections::HashMap;
use tokio::sync::Mutex;

use crate::config::Config;

pub struct Nodes {
    docker_api_port: u16,
    cert_dir: std::path::PathBuf,
    local: bool,
    clients: Mutex<HashMap<String, Docker>>,
}

impl Nodes {
    pub fn new(config: &Config, nodes: &NodesFile) -> Self {
        Self {
            docker_api_port: nodes.docker_api_port,
            cert_dir: config.docker_cert_dir.clone(),
            local: config.docker_local,
            clients: Mutex::new(HashMap::new()),
        }
    }

    pub async fn client_for(&self, node: &Node) -> Result<Docker> {
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(&node.name) {
            return Ok(client.clone());
        }
        if self.local {
            // Smoke-test mode: every "node" is the local daemon.
            let docker =
                Docker::connect_with_local_defaults().context("connecting to local Docker")?;
            clients.insert(node.name.clone(), docker.clone());
            return Ok(docker);
        }
        let addr = format!("tcp://{}:{}", node.wireguard_ip, self.docker_api_port);
        let docker = Docker::connect_with_ssl(
            &addr,
            &self.cert_dir.join("reconciler-key.pem"),
            &self.cert_dir.join("reconciler-cert.pem"),
            &self.cert_dir.join("ca.pem"),
            30,
            bollard::API_DEFAULT_VERSION,
        )
        .with_context(|| format!("connecting to Docker on {} ({addr})", node.name))?;
        clients.insert(node.name.clone(), docker.clone());
        Ok(docker)
    }
}
