//! Permanent delete of an archived app (§16-adjacent, the one sanctioned escape
//! from "archive, never delete"). The bot has already moved the app's manifests
//! to `archived/<app>/` and taken it out of the rendered set; this reaps the
//! physical footprint git can't: any residual containers, every named volume,
//! and the managed database + its role. The shared per-project role is left
//! intact. Idempotent — safe to re-run if a later step (repo delete) fails.

use anyhow::{Context, Result};
use majnet_common::manifest::AppManifest;
use majnet_common::platform::NodesFile;
use majnet_common::EnvClass;

use crate::deploy::{self, DeployCtx};
use crate::AppState;

/// Purge the runtime + data of an archived app across every class it had.
pub async fn purge_app(state: &AppState, org: &str, app: &str) -> Result<Vec<String>> {
    let project = crate::rename::resolve_project(state, org).await?;

    // The archived manifests (moved by the bot) describe the volumes + DB engine.
    let ops = crate::snapshot::fetch(&state.http, &state.config, org, "ops", "main")
        .await?
        .context("ops main snapshot unavailable")?;
    let base = ops
        .files
        .get(&format!("archived/{app}/base.yaml"))
        .with_context(|| format!("{app} is not archived (no archived/{app}/base.yaml)"))?;
    let manifest = AppManifest::parse(std::str::from_utf8(base)?)?;
    let classes: Vec<EnvClass> = EnvClass::ALL
        .iter()
        .copied()
        .filter(|c| {
            ops.files
                .contains_key(&format!("archived/{app}/{}.yaml", c.as_str()))
        })
        .collect();

    let platform = crate::snapshot::fetch(
        &state.http,
        &state.config,
        &state.config.root_org,
        "platform",
        "main",
    )
    .await?
    .context("platform snapshot unavailable")?;
    let nodes = NodesFile::parse(platform.files.get("nodes.yaml").context("no nodes.yaml")?)?;

    let mut purged = Vec::new();
    for class in classes {
        let Some(node) = nodes.by_role(class.node_role()) else {
            continue;
        };
        let docker = state.nodes(&nodes).client_for(node).await?;
        let ctx = DeployCtx {
            docker: &docker,
            project: &project,
            class,
            commit: "imperative-purge",
            dry_run: state.config.dry_run,
            http: &state.http,
            bot_url: &state.config.bot_url,
        };

        // Residual containers (usually already gc'd once un-rendered).
        deploy::remove_app(&ctx, app).await?;

        for vol in &manifest.volumes {
            deploy::remove_volume(
                &docker,
                &deploy::volume_name(&project, app, class, &vol.name),
            )
            .await?;
        }
        if let Some(db) = &manifest.database {
            crate::db::drop_database(
                &docker,
                db.engine,
                &crate::db::db_name(&project, app, class),
            )
            .await?;
        }
        purged.push(class.as_str().to_string());
    }

    state.store.record(
        "imperative-purge",
        &project,
        "",
        &format!("purge {app}"),
        &purged.join(","),
    )?;
    Ok(purged)
}
