//! Convergence loop (§12) — the event-loop body from the design:
//!
//! ```text
//! snapshots = fetch from bot (platform + each project's env/* branches)
//! for each registered project:
//!   ensure networks on assigned nodes
//!   for each rendered manifest (app × class):
//!     validate → decrypt (class key) → container spec + node
//!     diff vs node's Docker state → migrations → blue-green converge
//!     record event {commit, project, node, action, result}
//! ```
//!
//! Per-app failures are recorded and skipped — one bad app never blocks the
//! rest of the fleet (but that app aborts loudly, no partial applies).

use anyhow::{Context, Result};
use bollard::query_parameters as qp;
use majnet_common::manifest::AppManifest;
use majnet_common::platform::{NodesFile, ProjectsFile};
use majnet_common::EnvClass;
use std::collections::BTreeMap;

use crate::deploy::{self, DeployCtx};
use crate::AppState;

const CLASSES: [EnvClass; 4] = [
    EnvClass::Testing,
    EnvClass::Stable,
    EnvClass::Production,
    EnvClass::Ephemeral,
];

pub async fn converge_all(state: &AppState) -> Result<()> {
    let platform = crate::snapshot::fetch(
        &state.http,
        &state.config,
        &state.config.root_org,
        "platform",
        "main",
    )
    .await?
    .context("platform repo snapshot unavailable")?;
    let nodes = NodesFile::parse(
        platform
            .files
            .get("nodes.yaml")
            .context("platform repo has no nodes.yaml")?,
    )?;
    let projects = ProjectsFile::parse(
        platform
            .files
            .get("projects.yaml")
            .context("platform repo has no projects.yaml")?,
    )?;

    tracing::info!(projects = projects.projects.len(), commit = %platform.commit, "converging");

    // Platform services (edge-main, …) onto their role's nodes — ADR 0007.
    // Non-fatal, and independent of any project.
    crate::platform::converge_platform(state, &nodes, &platform).await;

    for project in &projects.projects {
        for class in CLASSES {
            if let Err(e) =
                converge_project_class(state, &nodes, &platform, &project.name, &project.org, class)
                    .await
            {
                tracing::error!(
                    project = project.name,
                    class = class.as_str(),
                    error = format!("{e:#}"),
                    "class convergence failed"
                );
            }
        }
    }
    Ok(())
}

async fn converge_project_class(
    state: &AppState,
    nodes: &NodesFile,
    platform: &crate::snapshot::Snapshot,
    project: &str,
    org: &str,
    class: EnvClass,
) -> Result<()> {
    let Some(snapshot) =
        crate::snapshot::fetch(&state.http, &state.config, org, "ops", &class.env_branch()).await?
    else {
        return Ok(()); // class not rendered yet for this project
    };

    let node = nodes
        .by_role(class.node_role())
        .with_context(|| format!("no node with role '{}' in nodes.yaml", class.node_role()))?;
    let docker = state.nodes(nodes).client_for(node).await?;

    ensure_network(&docker, project, state.config.dry_run).await?;

    // VPN-only classes are served through the project's tailnet ingress (§7).
    // Local smoke tests have no tailnet — skip.
    if class.node_role() == "private" && !state.config.docker_local {
        if let Err(e) = crate::ingress::ensure_ingress(state, &docker, project, platform).await {
            // Ingress trouble must not block app convergence — apps still
            // deploy; access returns when the ingress recovers.
            tracing::error!(project, error = format!("{e:#}"), "ingress ensure failed");
        }
    }

    let ctx = DeployCtx {
        docker: &docker,
        project,
        class,
        commit: &snapshot.commit,
        dry_run: state.config.dry_run,
        http: &state.http,
        bot_url: &state.config.bot_url,
    };

    // Rendered env branch layout (§9): `<app>.yaml` at root, `secrets/<app>.yaml`.
    let manifests: BTreeMap<&str, &Vec<u8>> = snapshot
        .files
        .iter()
        .filter_map(|(path, content)| {
            Some((
                path.strip_suffix(".yaml").filter(|p| !p.contains('/'))?,
                content,
            ))
        })
        .collect();

    let mut converged_apps = Vec::new();
    for (app, content) in &manifests {
        // Hard TTL (§8): an ephemeral stack dies at 7 days even if its
        // manifest lingers (safety net for PRs that never close).
        if class == EnvClass::Ephemeral && state.store.ephemeral_ttl_expired(project, app)? {
            if !state.config.dry_run {
                deploy::remove_app(&ctx, app).await?;
                state.store.ephemeral_forget(project, app)?;
            }
            state.store.record(
                &snapshot.commit,
                project,
                &node.name,
                &format!("gc {app}"),
                "7 d hard TTL",
            )?;
            tracing::info!(project, app, "ephemeral stack hit 7 d hard TTL");
            continue; // deliberately not in the keep-list
        }

        let result = converge_one(state, &ctx, &snapshot, app, content).await;
        match result {
            Ok(summary) => {
                tracing::info!(project, class = class.as_str(), app, %summary, "converged");
                // Record only meaningful changes — a steady-state "in sync"
                // every poll would flood the event feed with no-op notifications.
                if summary != "in sync" {
                    state.store.record(
                        &snapshot.commit,
                        project,
                        &node.name,
                        &format!("converge {app}"),
                        &summary,
                    )?;
                }
                converged_apps.push(app.to_string());
                if class == EnvClass::Ephemeral {
                    state.store.ephemeral_mark_seen(project, app)?;
                }
            }
            Err(e) => {
                // Loud abort for this app; the rest continue. The app stays
                // in the GC keep-list — a failed deploy must not remove the
                // old (still serving) container.
                tracing::error!(
                    project,
                    class = class.as_str(),
                    app,
                    error = format!("{e:#}"),
                    "app convergence failed"
                );
                state.store.record(
                    &snapshot.commit,
                    project,
                    &node.name,
                    &format!("converge {app}"),
                    &format!("FAILED: {e:#}"),
                )?;
                converged_apps.push(app.to_string());
            }
        }
    }

    // Deletions only when config is gone from git (§12); ephemeral gets the
    // 48 h grace instead of immediate removal.
    let removed = if class == EnvClass::Ephemeral {
        crate::gc::ephemeral_gc(state, &ctx, &converged_apps).await?
    } else {
        deploy::gc_removed_apps(&ctx, &converged_apps).await?
    };
    for entry in removed {
        state
            .store
            .record(&snapshot.commit, project, &node.name, "gc", &entry)?;
        tracing::info!(project, class = class.as_str(), entry, "removed");
    }
    Ok(())
}

async fn converge_one(
    state: &AppState,
    ctx: &DeployCtx<'_>,
    snapshot: &crate::snapshot::Snapshot,
    app: &str,
    manifest_bytes: &[u8],
) -> Result<String> {
    // Re-validate defensively (§12.2) — manifests arrive final from the bot,
    // but the reconciler trusts nothing it didn't check.
    let yaml = std::str::from_utf8(manifest_bytes).context("manifest is not UTF-8")?;
    let manifest = AppManifest::parse(yaml).context("rendered manifest failed validation")?;
    anyhow::ensure!(
        manifest.name == app,
        "manifest name '{}' does not match file name '{app}'",
        manifest.name
    );

    let secrets = match snapshot.files.get(&format!("secrets/{app}.yaml")) {
        Some(encrypted) => {
            Some(crate::secrets::decrypt(&state.config, ctx.class, encrypted).await?)
        }
        None => {
            anyhow::ensure!(
                manifest.secrets.is_empty(),
                "manifest declares secrets but env branch has none"
            );
            None
        }
    };

    // Managed database (§15): deploy the engine on this node on first use, then
    // provision the logical DB — both before the app (and its migrations) run.
    let extra_env = match &manifest.database {
        Some(db) => {
            if !ctx.dry_run {
                crate::platform::ensure_engine(&state.config, ctx.docker, db.engine)
                    .await
                    .with_context(|| {
                        format!(
                            "ensuring {:?} engine on the {} node",
                            db.engine,
                            ctx.class.node_role()
                        )
                    })?;
            }
            crate::db::ensure(
                &state.config,
                ctx.docker,
                ctx.project,
                app,
                ctx.class,
                db.engine,
                ctx.dry_run,
            )
            .await?
        }
        None => Vec::new(),
    };

    deploy::converge_app(ctx, &manifest, secrets.as_ref(), &extra_env).await
}

async fn ensure_network(docker: &bollard::Docker, project: &str, dry_run: bool) -> Result<()> {
    let name = deploy::network_name(project);
    let existing = docker
        .list_networks(Some(qp::ListNetworksOptions {
            filters: Some([("name".to_string(), vec![name.clone()])].into()),
        }))
        .await?;
    if existing
        .iter()
        .any(|n| n.name.as_deref() == Some(name.as_str()))
    {
        return Ok(());
    }
    if dry_run {
        tracing::info!(network = name, "DRY RUN: would create network");
        return Ok(());
    }
    docker
        .create_network(bollard::models::NetworkCreateRequest {
            name: name.clone(),
            labels: Some([(deploy::LABEL_PROJECT.to_string(), project.to_string())].into()),
            ..Default::default()
        })
        .await
        .with_context(|| format!("creating network {name}"))?;
    tracing::info!(network = name, "created project network");
    Ok(())
}
