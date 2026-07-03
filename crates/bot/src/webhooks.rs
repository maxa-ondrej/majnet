//! GitHub webhook intake (§11.3) — HMAC-verified, deduped, dispatched.
//!
//! Events handled in phase 1:
//! - `registry_package` (GHCR publish) → digest bump on the project ops repo
//!   (ADR 0001: the native package webhook *is* the "GHA → bot" notification)
//! - `push` to `env/<class>` branches of an ops repo, or to the root platform
//!   repo → notify the reconciler
//! - `ping` → 200
//! - `pull_request` → logged only (ephemeral lifecycle lands in phase 4)

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::Arc;

use crate::AppState;

/// Constant-time verification of `X-Hub-Signature-256: sha256=<hex>`.
pub fn verify_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    let Some(hex_sig) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_sig) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, &'static str) {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("");

    if !verify_signature(&state.config.webhook_secret, &body, header("x-hub-signature-256")) {
        tracing::warn!("webhook signature verification failed");
        return (StatusCode::UNAUTHORIZED, "bad signature");
    }

    let event = header("x-github-event").to_string();
    let delivery = header("x-github-delivery").to_string();
    match state.store.record_delivery(&delivery) {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!(delivery, "duplicate delivery, skipping");
            return (StatusCode::OK, "duplicate");
        }
        Err(e) => {
            tracing::error!(error = %e, "delivery dedup failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "storage error");
        }
    }

    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON"),
    };

    // Dispatch in the background: GitHub expects a fast 2xx, and our handlers
    // do API round-trips of their own.
    let state2 = state.clone();
    tokio::spawn(async move {
        if let Err(e) = dispatch(&state2, &event, payload).await {
            tracing::error!(event, error = format!("{e:#}"), "webhook handling failed");
        }
    });

    (StatusCode::OK, "ok")
}

async fn dispatch(state: &AppState, event: &str, payload: serde_json::Value) -> anyhow::Result<()> {
    let org = payload["organization"]["login"].as_str().unwrap_or_default().to_string();
    match event {
        "ping" => {}
        "registry_package" | "package" => {
            crate::digest::on_package_published(state, &org, &payload).await?;
        }
        "push" => on_push(state, &org, &payload).await?,
        "pull_request" => {
            let action = payload["action"].as_str().unwrap_or_default();
            let number = payload["number"].as_u64().unwrap_or_default();
            tracing::info!(org, action, number, "pull_request event (phase 4: ephemeral lifecycle)");
        }
        other => tracing::debug!(event = other, "ignoring event"),
    }
    Ok(())
}

async fn on_push(state: &AppState, org: &str, payload: &serde_json::Value) -> anyhow::Result<()> {
    let repo = payload["repository"]["name"].as_str().unwrap_or_default();
    let git_ref = payload["ref"].as_str().unwrap_or_default();
    let commit = payload["after"].as_str().unwrap_or_default();
    let branch = git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref);

    let is_env_branch = repo == "ops" && branch.starts_with("env/");
    let is_platform = org == state.config.root_org && repo == "platform" && branch == "main";
    let is_ops_main = repo == "ops" && branch == "main";
    if is_env_branch || is_platform {
        tracing::info!(org, repo, branch, commit, "deployable push — notifying reconciler");
        state.store.log_event("push", Some(org), &format!("{repo}@{branch} {commit}"))?;
        crate::notify::notify_reconciler(state, org, repo, branch, commit).await;
        if is_platform {
            // Registry or people may have changed: full reconciliation.
            crate::org_sync::sync_all(state).await?;
        }
    } else if is_ops_main {
        tracing::info!(org, commit, "ops main push — rendering + org sync");
        crate::render::on_ops_main_push(state, org, commit).await?;
        // project.yaml may have changed: reconcile this org (repos, teams, ACLs).
        let (_, platform_tar) = crate::proxy::fetch_snapshot(state, &state.config.root_org, "platform", "main").await?;
        let platform = majnet_common::tarball::untar(&platform_tar)?;
        crate::org_sync::sync_org(state, org, &platform).await?;
    } else {
        tracing::debug!(org, repo, branch, "push ignored (not ops main/env or platform config)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::verify_signature;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn accepts_valid_signature() {
        let sig = sign("s3cret", b"{}");
        assert!(verify_signature("s3cret", b"{}", &sig));
    }

    #[test]
    fn rejects_wrong_secret_body_or_format() {
        let sig = sign("s3cret", b"{}");
        assert!(!verify_signature("other", b"{}", &sig));
        assert!(!verify_signature("s3cret", b"{...}", &sig));
        assert!(!verify_signature("s3cret", b"{}", "sha1=abcd"));
        assert!(!verify_signature("s3cret", b"{}", "sha256=nothex"));
        assert!(!verify_signature("s3cret", b"{}", ""));
    }
}
