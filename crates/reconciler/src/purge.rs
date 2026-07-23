//! Permanent delete of an archived app (§16-adjacent, the one sanctioned escape
//! from "archive, never delete"). The bot has already moved the app's manifests
//! to `archived/<app>/` and taken it out of the rendered set; this reaps the
//! physical footprint git can't: any residual containers, every named volume,
//! and the managed database + its role. The shared per-project role is left
//! intact. Idempotent — safe to re-run if a later step (repo delete) fails.

use anyhow::{Context, Result};
use majnet_common::manifest::{AppManifest, DbEngine};
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
            wireguard_ip: "",
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

/// Purge an ENTIRE archived project: every app's runtime + data, then the
/// per-project resources git can't express — the Docker network, the ingress
/// containers + tailscale state volume, and the shared `project_role` on each
/// relational engine. The bot has already emptied `project.yaml` and moved every
/// app to `archived/<app>/`. Returns the apps purged. The per-project teardown is
/// best-effort (parked nodes, absent resources): failures are logged, not fatal —
/// a whole-project delete must make progress even if a node is unreachable.
pub async fn purge_project(state: &AppState, org: &str) -> Result<Vec<String>> {
    let project = crate::rename::resolve_project(state, org).await?;

    let ops = crate::snapshot::fetch(&state.http, &state.config, org, "ops", "main")
        .await?
        .context("ops main snapshot unavailable")?;
    let apps: Vec<String> = ops
        .files
        .keys()
        .filter_map(|p| p.strip_prefix("archived/")?.strip_suffix("/base.yaml"))
        .map(str::to_string)
        .collect();

    // 1. Purge each archived app (containers, volumes, app DBs + app roles).
    let mut purged = Vec::new();
    for app in &apps {
        match purge_app(state, org, app).await {
            Ok(_) => purged.push(app.clone()),
            Err(e) => tracing::error!(
                org,
                app,
                error = format!("{e:#}"),
                "purge_app failed during project delete (continuing)"
            ),
        }
    }

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

    // 2. Per-node teardown: ingress containers + tailscale state volume, then the
    //    project network (after its containers are gone). Best-effort.
    for node in &nodes.nodes {
        let Ok(docker) = state.nodes(&nodes).client_for(node).await else {
            continue;
        };
        for name in [
            format!("proj-{project}-tunnel"),
            format!("proj-{project}-ingress"),
            format!("proj-{project}-tailscale"),
        ] {
            if let Err(e) = deploy::remove_container_if_exists(&docker, &name).await {
                tracing::warn!(
                    node = node.name,
                    name,
                    error = format!("{e:#}"),
                    "ingress remove"
                );
            }
        }
        if let Err(e) = deploy::remove_volume(&docker, &format!("proj-{project}-ts-state")).await {
            tracing::warn!(
                node = node.name,
                error = format!("{e:#}"),
                "ts-state volume remove"
            );
        }
        if let Err(e) = deploy::remove_network(&docker, &project).await {
            tracing::warn!(node = node.name, error = format!("{e:#}"), "network remove");
        }
    }

    // 3. Drop the shared project login role on each class's relational engine.
    //    Best-effort — the role/engine may not exist (no DB-backed apps).
    for class in EnvClass::ALL {
        let Some(node) = nodes.by_role(class.node_role()) else {
            continue;
        };
        let Ok(docker) = state.nodes(&nodes).client_for(node).await else {
            continue;
        };
        let role = crate::db::project_role(&project, class);
        for engine in [DbEngine::Postgres, DbEngine::Mariadb] {
            if let Err(e) = crate::db::drop_role(&docker, engine, &role).await {
                tracing::warn!(
                    class = class.as_str(),
                    role,
                    error = format!("{e:#}"),
                    "drop project role"
                );
            }
        }
    }

    state.store.record(
        "imperative-purge",
        &project,
        "",
        "purge-project",
        &purged.join(","),
    )?;
    Ok(purged)
}
