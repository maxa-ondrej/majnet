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
use majnet_common::manifest::{AppManifest, DbEngine};
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

    // DB name → the owning project's human role + derived password, collected
    // while converging each app (production/Postgres only). Feeds the per-project
    // Adminer auto-login (ADR 0014), converged after the loop once complete.
    let mut adminer_creds: BTreeMap<String, (String, String)> = BTreeMap::new();

    for project in &projects.projects {
        for class in CLASSES {
            if let Err(e) = converge_project_class(
                state,
                &nodes,
                &platform,
                &project.name,
                &project.org,
                class,
                &mut adminer_creds,
            )
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

    // Per-project Adminer (ADR 0014) — after the loop, with the full credential
    // map. Non-fatal, like the other platform services.
    if let Err(e) = crate::platform::converge_adminer(state, &nodes, &adminer_creds).await {
        tracing::error!(error = format!("{e:#}"), "adminer convergence failed");
        let _ = state.store.record(
            &platform.commit,
            "platform",
            "prod",
            "adminer",
            &format!("FAILED: {e:#}"),
        );
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
    adminer_creds: &mut BTreeMap<String, (String, String)>,
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

    ensure_network(&docker, project, class, state.config.dry_run).await?;

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

    // VPN-only classes are served through the project's tailnet ingress (§7).
    // Local smoke tests have no tailnet — skip.
    if class.node_role() == "private" && !state.config.docker_local {
        // Apps opting into public exposure via a Cloudflare Tunnel (ADR 0026):
        // their public hostnames drive the ingress's cloudflared sidecar.
        let public_hosts: Vec<String> = manifests
            .values()
            .filter_map(|c| std::str::from_utf8(c).ok())
            .filter_map(|s| AppManifest::parse(s).ok())
            .filter_map(|m| m.ingress.filter(|i| i.public).and_then(|i| i.host))
            .collect();
        if let Err(e) =
            crate::ingress::ensure_ingress(state, &docker, project, platform, &public_hosts).await
        {
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
        wireguard_ip: &node.wireguard_ip,
    };

    // In-flight renames for this project+class: convergence + GC skip both the
    // old and new names until the data migration completes (see `rename`).
    let frozen = state.store.renames_pending(project, class.as_str())?;

    let mut converged_apps = Vec::new();
    for (app, content) in &manifests {
        // Don't create the new stack until its data has been migrated.
        if frozen.iter().any(|(_, n)| n.as_str() == *app) {
            tracing::info!(
                project,
                class = class.as_str(),
                app,
                "frozen for in-flight rename — skipping convergence"
            );
            continue;
        }
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

        let result = converge_one(state, &ctx, app, content, adminer_creds).await;
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

    // Keep frozen OLD apps in the GC keep-list: a rename in flight must not
    // remove the still-serving old container before its data is migrated.
    for (old, _) in &frozen {
        if !converged_apps.iter().any(|a| a == old) {
            converged_apps.push(old.clone());
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
    // Drop build-info rows for apps no longer present in this class (GC'd,
    // renamed away, or archived) so they don't linger past their containers.
    if !state.config.dry_run {
        state
            .store
            .app_info_prune(project, class.as_str(), &converged_apps)?;
    }
    Ok(())
}

async fn converge_one(
    state: &AppState,
    ctx: &DeployCtx<'_>,
    app: &str,
    manifest_bytes: &[u8],
    adminer_creds: &mut BTreeMap<String, (String, String)>,
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

    // Secrets delivered as tmpfs files (§14, ADR 0024): the inline `secrets:` map
    // (majnet: envelopes) decrypted here. SOPS was fully retired (ADR 0024 phase 3);
    // a legacy bare-name declaration has no delivery path, so fail loudly rather than
    // deploy an app missing its secrets.
    let delivered = if let Some(inline) = manifest.secrets.inline() {
        crate::secrets::decrypt_inline(&state.config, ctx.class, inline).await?
    } else if let Some(names) = manifest.secrets.names() {
        anyhow::bail!(
            "app declares legacy `secrets:` names {names:?} but SOPS delivery was removed \
             (ADR 0024) — set them as an inline `secrets:` map"
        )
    } else {
        BTreeMap::new()
    };
    let secrets = (!delivered.is_empty()).then_some(delivered);

    // Managed database (§15): deploy the engine on this node on first use, then
    // provision the logical DB — both before the app (and its migrations) run.
    let mut extra_env = match &manifest.database {
        Some(db) => {
            // Per-project Adminer auto-login map (ADR 0014): the prod Adminer
            // browses `majnet-postgres`, so collect only production Postgres DBs
            // → the project's human role + derived password (scoped to the
            // project's own databases).
            if ctx.class == EnvClass::Production && db.engine == DbEngine::Postgres {
                if let Ok((role, password)) =
                    crate::db::project_credentials(&state.config, db.engine, ctx.project, ctx.class)
                {
                    adminer_creds.insert(
                        crate::db::db_name(ctx.project, app, ctx.class),
                        (role, password),
                    );
                }
            }
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

    // OpenTelemetry (ADR 0023): inject the OTLP endpoint + resource attributes
    // when the app opts in and the platform has a collector. Folded into the
    // config hash via extra_env, so toggling `otel` re-converges the app.
    extra_env.extend(otel_env(
        manifest.otel,
        state.config.otlp_endpoint.as_deref(),
        ctx.project,
        app,
        ctx.class,
    ));

    // Track the rollout's stages (deploy trackability). converge_app writes
    // stages only past its in-sync/dry-run short-circuit; here we cap it with
    // done/failed so the dashboard sees a terminal state.
    let tracker =
        crate::state::DeployTracker::new(&state.store, ctx.project, app, ctx.class.as_str());
    let summary =
        match deploy::converge_app(ctx, &manifest, secrets.as_ref(), &extra_env, &tracker).await {
            Ok(s) => s,
            Err(e) => {
                tracker.fail(&format!("{e:#}"));
                return Err(e);
            }
        };
    // On an actual rollout (not a no-op "in sync"), mark it deployed, then scrape
    // the app's standard `/info` now that the health gate has proven it serves
    // HTTP, and record the build metadata for the dashboard. Best-effort — never
    // fails the deploy. Label the deploy event with the reported version instead
    // of the raw image digest when the app reports one.
    if summary.starts_with("deployed") {
        tracker.done(&summary);
        if !ctx.dry_run {
            if let Some(version) = crate::info::capture(state, ctx, &manifest).await {
                return Ok(summary.replace(&manifest.image_ref(), &version));
            }
        }
    }
    Ok(summary)
}

/// Ensure both the shared per-project infra network and the per-class app network
/// exist (ADR 0027). Called once per project+class; the shared net is idempotent.
async fn ensure_network(
    docker: &bollard::Docker,
    project: &str,
    class: EnvClass,
    dry_run: bool,
) -> Result<()> {
    ensure_named_network(docker, &deploy::network_name(project), project, dry_run).await?;
    ensure_named_network(
        docker,
        &deploy::class_network_name(project, class),
        project,
        dry_run,
    )
    .await
}

async fn ensure_named_network(
    docker: &bollard::Docker,
    name: &str,
    project: &str,
    dry_run: bool,
) -> Result<()> {
    let existing = docker
        .list_networks(Some(qp::ListNetworksOptions {
            filters: Some([("name".to_string(), vec![name.to_string()])].into()),
        }))
        .await?;
    if existing.iter().any(|n| n.name.as_deref() == Some(name)) {
        return Ok(());
    }
    if dry_run {
        tracing::info!(network = name, "DRY RUN: would create network");
        return Ok(());
    }
    docker
        .create_network(bollard::models::NetworkCreateRequest {
            name: name.to_string(),
            labels: Some([(deploy::LABEL_PROJECT.to_string(), project.to_string())].into()),
            ..Default::default()
        })
        .await
        .with_context(|| format!("creating network {name}"))?;
    tracing::info!(network = name, "created project network");
    Ok(())
}

/// OTEL env to inject when an app opts in (`otel: true`) and the platform has a
/// collector endpoint configured (ADR 0023). Empty when either is missing — so
/// `otel` is inert until the backend exists. Resource attributes tag every
/// signal by app / environment / project; the app's OTEL SDK supplies the rest.
fn otel_env(
    otel: bool,
    endpoint: Option<&str>,
    project: &str,
    app: &str,
    class: EnvClass,
) -> Vec<(String, String)> {
    match (otel, endpoint) {
        (true, Some(ep)) if !ep.is_empty() => vec![
            ("OTEL_EXPORTER_OTLP_ENDPOINT".to_string(), ep.to_string()),
            ("OTEL_SERVICE_NAME".to_string(), app.to_string()),
            (
                "OTEL_RESOURCE_ATTRIBUTES".to_string(),
                format!(
                    "service.name={app},deployment.environment={},project={project}",
                    class.as_str()
                ),
            ),
        ],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otel_env_is_inert_unless_opted_in_and_endpoint_set() {
        // No opt-in, or no endpoint → nothing injected (safe before the backend).
        assert!(otel_env(
            false,
            Some("http://otel-collector:4317"),
            "sideline",
            "sideline-server",
            EnvClass::Production
        )
        .is_empty());
        assert!(otel_env(
            true,
            None,
            "sideline",
            "sideline-server",
            EnvClass::Production
        )
        .is_empty());
        assert!(otel_env(
            true,
            Some(""),
            "sideline",
            "sideline-server",
            EnvClass::Production
        )
        .is_empty());
    }

    #[test]
    fn otel_env_injects_endpoint_and_tagged_attributes() {
        let env = otel_env(
            true,
            Some("http://otel-collector:4317"),
            "sideline",
            "sideline-server",
            EnvClass::Production,
        );
        let m: std::collections::BTreeMap<_, _> = env.into_iter().collect();
        assert_eq!(
            m["OTEL_EXPORTER_OTLP_ENDPOINT"],
            "http://otel-collector:4317"
        );
        assert_eq!(m["OTEL_SERVICE_NAME"], "sideline-server");
        assert_eq!(
            m["OTEL_RESOURCE_ATTRIBUTES"],
            "service.name=sideline-server,deployment.environment=production,project=sideline"
        );
    }
}
