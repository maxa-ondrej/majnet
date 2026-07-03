//! Repo access proxy (§11.8): the reconciler holds no GitHub credentials, so
//! the bot serves it snapshots of the rendered `env/*` branches and the root
//! platform repo over the WG-internal API.
//!
//! GET /api/snapshot/{org}/{repo}/{branch}
//!   → 200, `application/gzip` tarball of the branch tip,
//!     `X-Majnet-Commit: <sha>` header. Cached on disk by commit SHA.

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::response::IntoResponse;
use secrecy::ExposeSecret;
use std::sync::Arc;

use crate::AppState;

pub static COMMIT_HEADER: HeaderName = HeaderName::from_static("x-majnet-commit");

pub async fn snapshot(
    State(state): State<Arc<AppState>>,
    Path((org, repo, branch)): Path<(String, String, String)>,
) -> impl IntoResponse {
    match fetch_snapshot(&state, &org, &repo, &branch).await {
        Ok((sha, bytes)) => {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, "application/gzip".parse().unwrap());
            headers.insert(COMMIT_HEADER.clone(), sha.parse().unwrap());
            (StatusCode::OK, headers, bytes).into_response()
        }
        Err(e) => {
            tracing::error!(
                org,
                repo,
                branch,
                error = format!("{e:#}"),
                "snapshot failed"
            );
            (StatusCode::BAD_GATEWAY, format!("snapshot failed: {e:#}")).into_response()
        }
    }
}

pub(crate) async fn fetch_snapshot(
    state: &AppState,
    org: &str,
    repo: &str,
    branch: &str,
) -> Result<(String, Vec<u8>)> {
    let (client, token) = state.github.org_client_and_token(org).await?;

    // Resolve branch → commit SHA (also the cache key).
    let sha = client
        .commits(org, repo)
        .get(branch)
        .await
        .with_context(|| format!("resolving {org}/{repo}@{branch}"))?
        .sha;

    let cache_dir = state.config.data_dir.join("snapshots");
    let cache_path = cache_dir.join(format!("{org}--{repo}--{sha}.tar.gz"));
    if let Ok(bytes) = tokio::fs::read(&cache_path).await {
        return Ok((sha, bytes));
    }

    let url = format!("https://api.github.com/repos/{org}/{repo}/tarball/{sha}");
    let bytes = state
        .http
        .get(&url)
        .bearer_auth(token.expose_secret())
        .header(header::USER_AGENT, "majnet-bot")
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("downloading tarball of {org}/{repo}@{sha}"))?
        .bytes()
        .await?
        .to_vec();

    tokio::fs::create_dir_all(&cache_dir).await?;
    // Write via temp + rename so a concurrent reader never sees a torn file.
    let tmp = cache_path.with_extension("tmp");
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, &cache_path).await?;
    tracing::info!(
        org,
        repo,
        branch,
        sha,
        size = bytes.len(),
        "snapshot cached"
    );
    Ok((sha, bytes))
}
