//! Platform repo seeding: read `platform-seed/` from the install checkout,
//! render what we already know (the main node's entry in nodes.yaml), and
//! hand the tree to the bot to commit (writes-through-git, ADR 0004).

use anyhow::{Context, Result};
use majnet_common::platform::NodesFile;
use std::collections::BTreeMap;
use std::path::Path;

use crate::state::SetupState;

/// The seed tree, with `nodes.yaml` rendered from wizard state.
pub fn build_tree(seed_dir: &Path, state: &SetupState) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    walk(seed_dir, seed_dir, &mut files)?;
    anyhow::ensure!(
        files.contains_key("nodes.yaml"),
        "platform-seed has no nodes.yaml at {}",
        seed_dir.display()
    );
    let rendered = render_nodes(&files["nodes.yaml"], state)?;
    files.insert("nodes.yaml".into(), rendered);
    Ok(files)
}

fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            walk(root, &path, files)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .expect("under root")
                .to_string_lossy()
                .replace('\\', "/");
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {} (seed files must be text)", path.display()))?;
            files.insert(rel, content);
        }
    }
    Ok(())
}

/// Fill in every node the wizard already knows (at seed time: main).
fn render_nodes(seed_yaml: &str, state: &SetupState) -> Result<String> {
    let mut nodes = NodesFile::parse(seed_yaml.as_bytes()).context("parsing seed nodes.yaml")?;
    for node in &mut nodes.nodes {
        if let Some(known) = state.nodes.get(&node.name) {
            node.public_endpoint = known.public_endpoint.clone();
            node.wireguard_pubkey = known.wireguard_pubkey.clone();
        }
    }
    Ok(format!(
        "# Managed by the platform — updated via node enrollment (ADR 0004).\n{}",
        serde_yaml::to_string(&nodes)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::NodeEntry;

    #[test]
    fn renders_known_nodes_into_seed() {
        let seed = "wireguard_subnet: 10.88.0.0/24\ndocker_api_port: 2376\nnodes:\n  - name: main\n    role: main\n    wireguard_ip: 10.88.0.1\n  - name: prod\n    role: prod\n    wireguard_ip: 10.88.0.2\n";
        let mut state = SetupState::default();
        state.nodes.insert(
            "main".into(),
            NodeEntry {
                role: "main".into(),
                ssh_host: String::new(),
                wireguard_ip: "10.88.0.1".into(),
                public_endpoint: "203.0.113.1:51820".into(),
                wireguard_pubkey: "PUBKEY".into(),
            },
        );
        let out = render_nodes(seed, &state).unwrap();
        let parsed = NodesFile::parse(out.as_bytes()).unwrap();
        assert_eq!(parsed.nodes[0].wireguard_pubkey, "PUBKEY");
        assert_eq!(parsed.nodes[0].public_endpoint, "203.0.113.1:51820");
        assert_eq!(parsed.nodes[1].wireguard_pubkey, ""); // prod not enrolled yet
    }
}
