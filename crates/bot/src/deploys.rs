//! Deployment requests (§16): the `env/<class>` render PRs on a project's ops
//! repo. Production render PRs await an admin merge — that merge **is** the
//! production review gate (§9). This module surfaces those PRs to the dashboard
//! and performs the merge/close on behalf of the authenticated admin, so the
//! review the design keeps in the UI runs through the bot (writes-through-git):
//! merging pushes `env/<class>`, which the reconciler then converges.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use majnet_common::project::Role;
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

use crate::AppState;

type ApiError = (StatusCode, String);

fn bad_gateway(e: impl std::fmt::Display) -> ApiError {
    (StatusCode::BAD_GATEWAY, format!("{e}"))
}

#[derive(Serialize)]
pub struct DeployPr {
    pub number: u64,
    pub title: String,
    /// Target class, from the `env/<class>` base branch.
    pub class: String,
    pub base: String,
    pub created_at: String,
    pub files: Vec<DeployFile>,
}

#[derive(Serialize)]
pub struct DeployFile {
    pub filename: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    /// Unified-diff hunk (absent for binary/large files).
    pub patch: Option<String>,
}

/// `GET /api/deploys/{org}` — open render PRs (base `env/*`) with their diffs.
pub async fn list(
    State(state): State<Arc<AppState>>,
    Path(org): Path<String>,
) -> Result<Json<Vec<DeployPr>>, ApiError> {
    let client = state.github.org_client(&org).await.map_err(bad_gateway)?;
    let repo = format!("/repos/{org}/ops");
    let prs: serde_json::Value = client
        .get(format!("{repo}/pulls?state=open"), None::<&()>)
        .await
        .map_err(bad_gateway)?;

    let mut out = Vec::new();
    for pr in prs.as_array().into_iter().flatten() {
        let base = pr["base"]["ref"].as_str().unwrap_or_default();
        // Only render PRs (onto env/<class>); ignore anything else humans opened.
        let Some(class) = base.strip_prefix("env/") else {
            continue;
        };
        let number = pr["number"].as_u64().unwrap_or_default();
        let files: serde_json::Value = client
            .get(format!("{repo}/pulls/{number}/files"), None::<&()>)
            .await
            .map_err(bad_gateway)?;
        let files = files
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| DeployFile {
                filename: f["filename"].as_str().unwrap_or_default().to_string(),
                status: f["status"].as_str().unwrap_or_default().to_string(),
                additions: f["additions"].as_u64().unwrap_or(0),
                deletions: f["deletions"].as_u64().unwrap_or(0),
                patch: f["patch"].as_str().map(str::to_string),
            })
            .collect();
        out.push(DeployPr {
            number,
            title: pr["title"].as_str().unwrap_or_default().to_string(),
            class: class.to_string(),
            base: base.to_string(),
            created_at: pr["created_at"].as_str().unwrap_or_default().to_string(),
            files,
        });
    }
    Ok(Json(out))
}

/// `POST /api/deploys/{org}/{number}/merge` — merge a render PR (the deploy
/// trigger). Production is admin-only (the §9 gate); other classes: developer.
pub async fn merge(
    State(state): State<Arc<AppState>>,
    Path((org, number)): Path<(String, u64)>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    let (client, repo, class) = resolve_render_pr(&state, &org, number).await?;
    let actor = gate(&state, &headers, &org, &class).await?;
    let _: serde_json::Value = client
        .put(
            format!("{repo}/pulls/{number}/merge"),
            Some(&json!({ "merge_method": "merge" })),
        )
        .await
        .map_err(bad_gateway)?;
    state
        .store
        .log_event(
            "deploy-merge",
            Some(&org),
            &format!("PR #{number} (env/{class}) by {actor}"),
        )
        .map_err(bad_gateway)?;
    Ok(format!(
        "merged PR #{number} → env/{class}; the reconciler will converge"
    ))
}

/// `POST /api/deploys/{org}/{number}/close` — reject a render PR without
/// deploying. Same role gate as merge (declining a production change is an
/// admin decision).
pub async fn close(
    State(state): State<Arc<AppState>>,
    Path((org, number)): Path<(String, u64)>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    let (client, repo, class) = resolve_render_pr(&state, &org, number).await?;
    let actor = gate(&state, &headers, &org, &class).await?;
    let _: serde_json::Value = client
        .patch(
            format!("{repo}/pulls/{number}"),
            Some(&json!({ "state": "closed" })),
        )
        .await
        .map_err(bad_gateway)?;
    state
        .store
        .log_event(
            "deploy-close",
            Some(&org),
            &format!("PR #{number} (env/{class}) by {actor}"),
        )
        .map_err(bad_gateway)?;
    Ok(format!("closed PR #{number} (env/{class}) — not deployed"))
}

/// Resolve a PR to its ops client + class, rejecting anything that isn't a
/// render PR onto `env/<class>` (so these endpoints can't merge arbitrary PRs).
async fn resolve_render_pr(
    state: &AppState,
    org: &str,
    number: u64,
) -> Result<(octocrab::Octocrab, String, String), ApiError> {
    let client = state.github.org_client(org).await.map_err(bad_gateway)?;
    let repo = format!("/repos/{org}/ops");
    let pr: serde_json::Value = client
        .get(format!("{repo}/pulls/{number}"), None::<&()>)
        .await
        .map_err(bad_gateway)?;
    let base = pr["base"]["ref"].as_str().unwrap_or_default();
    let class = base.strip_prefix("env/").ok_or((
        StatusCode::BAD_REQUEST,
        format!("PR #{number} is not a render PR (base {base})"),
    ))?;
    Ok((client, repo, class.to_string()))
}

/// Enforce the class's role: production → admin, otherwise developer.
async fn gate(
    state: &AppState,
    headers: &HeaderMap,
    org: &str,
    class: &str,
) -> Result<String, ApiError> {
    let min_role = if class == "production" {
        Role::Admin
    } else {
        Role::Developer
    };
    crate::authz::require(state, headers, org, min_role)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))
}
