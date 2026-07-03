//! Ephemeral GC (§8, §13): previews outlive their manifest by a 48 h grace
//! period (the PR just closed — logs and state may still be wanted), and
//! nothing ephemeral survives past 7 days (hard TTL, enforced in the
//! converge loop even while the manifest lingers).
//!
//! Stable/production use the immediate `deploy::gc_removed_apps` path — the
//! grace period is an ephemeral-only concession.
//!
//! Lifecycle (docs/diagrams/lifecycles.puml):
//!   Deployed → Grace (manifest removed) → GC after 48 h
//!   Deployed → GC at 7 d hard TTL or manual removal

use anyhow::Result;

use crate::deploy::{self, DeployCtx};
use crate::AppState;

/// Ephemeral GC pass: track presence, remove what's out of grace.
pub async fn ephemeral_gc(
    state: &AppState,
    ctx: &DeployCtx<'_>,
    rendered_apps: &[String],
) -> Result<Vec<String>> {
    // Containers whose manifest vanished start (or continue) their countdown.
    for app in deploy::list_class_apps(ctx).await? {
        if !rendered_apps.contains(&app) {
            state.store.ephemeral_mark_missing(ctx.project, &app)?;
        }
    }

    let mut removed = Vec::new();
    for app in state.store.ephemeral_grace_expired(ctx.project)? {
        if ctx.dry_run {
            removed.push(format!("DRY RUN: would GC {app} (grace expired)"));
            continue;
        }
        deploy::remove_app(ctx, &app).await?;
        state.store.ephemeral_forget(ctx.project, &app)?;
        removed.push(format!("{app} (48 h grace expired)"));
    }
    Ok(removed)
}
