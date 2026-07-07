//! WG-internal platform provisioning API (ADR 0004): the setup service holds
//! no GitHub credentials, so its platform-repo writes land here and the bot
//! authors the commits — writes-through-git stays intact (§6).

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use base64::Engine;
use majnet_common::platform::{Node, NodesFile, VersionFile};
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

    // Ensure the repo exists by attempting creation and tolerating 422
    // ("name already exists"). We deliberately do NOT read-then-create: an
    // installation token can return 404 on `GET /repos/{org}/platform` for a
    // repo it just created, which made the old `repo_exists` guard loop —
    // seeing "absent", trying to create, and failing the create with 422.
    tracing::info!(org, "ensuring platform repo exists");
    match client
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
    {
        Ok::<serde_json::Value, _>(_) => tracing::info!(org, "created platform repo"),
        Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 422 => {
            tracing::info!(org, "platform repo already exists — reusing")
        }
        Err(e) => return Err(e).context("creating platform repo"),
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

/// `GET /api/platform/version` — the control-plane version pin from
/// `version.yaml` on platform `main` (ADR 0005). Plain text: the consumer
/// is `majnet-update`, a shell script.
pub async fn version(State(state): State<Arc<AppState>>) -> Result<String, (StatusCode, String)> {
    do_version(&state)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn do_version(state: &AppState) -> Result<String> {
    let org = &state.config.root_org;
    let client = state.github.org_client(org).await?;
    let yaml = read_platform_file(&client, org, "version.yaml").await?;
    let pin = VersionFile::parse(yaml.as_bytes()).context("parsing version.yaml")?;
    Ok(pin.control_plane.git_ref)
}

async fn read_platform_file(client: &octocrab::Octocrab, org: &str, path: &str) -> Result<String> {
    let content = client
        .repos(org, "platform")
        .get_content()
        .path(path)
        .r#ref("main")
        .send()
        .await
        .with_context(|| format!("reading {path} — is the platform repo seeded?"))?;
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
    Ok(String::from_utf8(decoded)?)
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
