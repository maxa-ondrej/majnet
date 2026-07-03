//! Root platform repo config (`majksa-platform/platform`) — §10.
//! Shapes match `platform-seed/*.yaml`.

use serde::{Deserialize, Serialize};

/// `nodes.yaml` — the three static nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodesFile {
    pub wireguard_subnet: String,
    pub docker_api_port: u16,
    pub nodes: Vec<Node>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    /// `main` | `prod` | `private` (= trust zone, §4).
    pub role: String,
    pub wireguard_ip: String,
    #[serde(default)]
    pub public_endpoint: String,
    #[serde(default)]
    pub wireguard_pubkey: String,
}

impl NodesFile {
    pub fn parse(yaml: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_yaml::from_slice(yaml)?)
    }

    pub fn by_role(&self, role: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.role == role)
    }
}

/// `people.yaml` — GitHub username ↔ Tailscale identity mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeopleFile {
    pub people: Vec<Person>,
}

impl PeopleFile {
    pub fn parse(yaml: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_yaml::from_slice(yaml)?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Person {
    pub github: String,
    pub tailscale: String,
    #[serde(default)]
    pub admin: bool,
}

/// `projects.yaml` — the registry that gates project discovery (§2).
/// A project exists only when the GitHub App is installed on the org
/// **and** the org appears here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectsFile {
    pub projects: Vec<ProjectRegistryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRegistryEntry {
    pub name: String,
    pub org: String,
}

impl ProjectsFile {
    pub fn parse(yaml: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_yaml::from_slice(yaml)?)
    }
}
