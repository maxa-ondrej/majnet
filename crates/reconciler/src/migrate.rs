//! App data migration (ADR 0010 phase 3) — restore a database dump into a
//! MajNet-provisioned engine, on the class's node, over the Docker API.
//!
//! This is the one imperative on-node step of a migration: data isn't
//! git-shaped, so it bypasses the render/converge loop. Idempotent — a
//! completed restore is recorded, and a re-upload is a no-op. Forward-only: a
//! failed restore is not recorded (so it's retryable), but a *partial* restore
//! left in a live DB is the operator's to reset before retrying.

use anyhow::{ensure, Context, Result};
use majnet_common::manifest::DbEngine;
use majnet_common::platform::NodesFile;
use majnet_common::EnvClass;

use crate::AppState;

/// Restore `dump` into `project/app`'s `class` database of `engine`.
pub async fn restore_db(
    state: &AppState,
    project: &str,
    app: &str,
    class: EnvClass,
    engine: DbEngine,
    dump: &[u8],
) -> Result<String> {
    if state.store.data_migration_done(project, app, class.as_str())? {
        return Ok(format!(
            "{project}/{app} ({}) already restored — skipping",
            class.as_str()
        ));
    }
    ensure!(!dump.is_empty(), "empty dump — nothing to restore");

    // Resolve the class's node exactly as convergence / restart do.
    let platform = crate::snapshot::fetch(
        &state.http,
        &state.config,
        &state.config.root_org,
        "platform",
        "main",
    )
    .await?
    .context("platform snapshot unavailable")?;
    let nodes = NodesFile::parse(
        platform
            .files
            .get("nodes.yaml")
            .context("no nodes.yaml")?,
    )?;
    let node = nodes
        .by_role(class.node_role())
        .context("no node for class")?;
    let docker = state.nodes(&nodes).client_for(node).await?;

    // Provision the engine + the app's DB/user, then restore into it.
    crate::platform::ensure_engine(&state.config, &docker, engine).await?;
    crate::db::ensure(&state.config, &docker, project, app, class, engine, false)
        .await
        .context("provisioning database before restore")?;
    crate::db::restore(&docker, project, app, class, engine, dump).await?;

    state
        .store
        .record_data_migration(project, app, class.as_str())?;
    state.store.record(
        "imperative",
        project,
        &node.name,
        &format!("data-restore {app} ({})", class.as_str()),
        &format!("{} bytes, {engine:?}", dump.len()),
    )?;
    tracing::info!(
        project,
        app,
        class = class.as_str(),
        ?engine,
        bytes = dump.len(),
        "data restored"
    );
    Ok(format!(
        "{project}/{app} ({}) restored: {} bytes into {engine:?}",
        class.as_str(),
        dump.len()
    ))
}
