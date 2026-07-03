//! Reconciler notification (§11.8) — a nudge over the WG-internal network;
//! the reconciler then pulls snapshots via the proxy. Best-effort by design:
//! the reconciler's ~5 min drift poll (§12.1) catches anything missed here.

use crate::AppState;

pub async fn notify_reconciler(
    state: &AppState,
    org: &str,
    repo: &str,
    branch: &str,
    commit: &str,
) {
    if state.config.reconciler_url.is_empty() {
        tracing::debug!("MAJNET_RECONCILER_URL unset — skipping notify");
        return;
    }
    let url = format!(
        "{}/notify",
        state.config.reconciler_url.trim_end_matches('/')
    );
    let body = serde_json::json!({ "org": org, "repo": repo, "branch": branch, "commit": commit });
    match state.http.post(&url).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => tracing::warn!(status = %resp.status(), "reconciler notify rejected"),
        Err(e) => tracing::warn!(error = %e, "reconciler notify failed (drift poll will catch up)"),
    }
}
