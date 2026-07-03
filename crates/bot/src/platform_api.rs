//! WG-internal platform provisioning API (ADR 0004): the setup service holds
//! no GitHub credentials, so its platform-repo writes land here and the bot
//! authors the commits — writes-through-git stays intact (§6).

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use base64::Engine;
use majnet_common::platform::{Node, NodesFile};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::AppState;

#[derive(Deserialize)]
pub struct SeedRequest {
    /// path → content of the initial `platform` repo tree.
    pub files: BTreeMap<String, String>,
}

/// `POST /api/platform/seed` — create the root `platform` repo from the seed
/// tree. One-shot: a repo that already has a `main` branch is left untouched.
pub async fn seed(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SeedRequest>,
) -> Result<String, (StatusCode, String)> {
    do_seed(&state, req)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn do_seed(state: &AppState, req: SeedRequest) -> Result<String> {
    anyhow::ensure!(!req.files.is_empty(), "seed tree is empty");
    let org = &state.config.root_org;
    let client = state.github.org_client(org).await?;
    let repo = format!("/repos/{org}/platform");

    if !crate::org_sync::repo_exists(&client, org, "platform").await? {
        tracing::info!(org, "creating platform repo");
        let _: serde_json::Value = client
            .post(
                format!("/orgs/{org}/repos"),
                Some(&json!({
                    "name": "platform",
                    "description": "MajNet platform config — nodes, people, project registry",
                    "private": true,
                    "auto_init": false,
                })),
            )
            .await
            .context("creating platform repo")?;
    }
    if crate::git::get_branch_head(&client, &repo, "main")
        .await?
        .is_some()
    {
        return Ok("platform repo already seeded — nothing done".into());
    }

    let tree = crate::git::create_tree(&client, &repo, &req.files).await?;
    let commit = crate::git::create_commit(
        &client,
        &repo,
        &tree,
        &[],
        "chore: seed platform repo (setup wizard)",
    )
    .await?;
    crate::git::create_ref(&client, &repo, "main", &commit).await?;
    state.store.log_event(
        "platform-seeded",
        Some(org),
        &format!("{} files", req.files.len()),
    )?;
    tracing::info!(org, files = req.files.len(), "platform repo seeded");
    Ok(format!("seeded {org}/platform ({} files)", req.files.len()))
}

/// `POST /api/platform/node` — upsert one entry in `nodes.yaml` on platform
/// `main` (node enrollment / WG pubkey + endpoint updates). Keyed by name.
pub async fn upsert_node(
    State(state): State<Arc<AppState>>,
    Json(node): Json<Node>,
) -> Result<String, (StatusCode, String)> {
    do_upsert_node(&state, node)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn do_upsert_node(state: &AppState, node: Node) -> Result<String> {
    anyhow::ensure!(
        matches!(node.role.as_str(), "main" | "prod" | "private"),
        "role must be main|prod|private, got: {}",
        node.role
    );
    anyhow::ensure!(
        !node.name.is_empty()
            && node
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "invalid node name"
    );

    let org = &state.config.root_org;
    let client = state.github.org_client(org).await?;
    let repos = client.repos(org, "platform");

    let (current, sha) = match repos
        .get_content()
        .path("nodes.yaml")
        .r#ref("main")
        .send()
        .await
    {
        Ok(content) => {
            let item = content
                .items
                .into_iter()
                .next()
                .context("empty contents response")?;
            let encoded = item
                .content
                .clone()
                .unwrap_or_default()
                .replace(['\n', ' '], "");
            let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
            (String::from_utf8(decoded)?, item.sha)
        }
        Err(e) => return Err(e).context("reading nodes.yaml — is the platform repo seeded?"),
    };

    let mut nodes = NodesFile::parse(current.as_bytes()).context("parsing nodes.yaml")?;
    let summary = format!(
        "{} ({}) wg={} endpoint={}",
        node.name, node.role, node.wireguard_ip, node.public_endpoint
    );
    match nodes.nodes.iter_mut().find(|n| n.name == node.name) {
        Some(existing) => *existing = node,
        None => nodes.nodes.push(node),
    }

    let updated = format!(
        "# Managed by the platform — updated via node enrollment (ADR 0004).\n{}",
        serde_yaml::to_string(&nodes)?
    );
    if updated == current {
        return Ok("nodes.yaml already up to date".into());
    }
    repos
        .update_file("nodes.yaml", format!("enroll: {summary}"), &updated, &sha)
        .branch("main")
        .send()
        .await
        .context("committing nodes.yaml")?;
    state
        .store
        .log_event("node-enrolled", Some(org), &summary)?;
    tracing::info!(%summary, "nodes.yaml updated");
    Ok(format!("nodes.yaml updated: {summary}"))
}
