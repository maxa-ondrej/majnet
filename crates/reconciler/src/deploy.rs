//! Blue-green deploy engine (§12.5, docs/diagrams/lifecycles.puml).
//!
//! Migrating → Starting → HealthCheck → Flipping → Draining → Done
//!                              ↘ Failed (old container keeps serving)
//!
//! The "flip" leans on Traefik's docker provider semantics: a container with
//! a HEALTHCHECK is only added to the router once healthy, so the new
//! container starts with its labels in place, takes traffic exactly when it
//! turns healthy, and the old one is stopped afterwards. A failed health
//! check tears the new container down and leaves the old serving.

use anyhow::{bail, Context, Result};
use bollard::models::{
    ContainerCreateBody, ContainerSummary, HealthConfig, HostConfig, RestartPolicy,
    RestartPolicyNameEnum,
};
use bollard::query_parameters as qp;
use bollard::Docker;
use futures_util::TryStreamExt;
use majnet_common::manifest::AppManifest;
use majnet_common::EnvClass;
use sha2::Digest;
use std::collections::{BTreeMap, HashMap};

pub struct DeployCtx<'a> {
    pub docker: &'a Docker,
    pub project: &'a str,
    pub class: EnvClass,
    pub commit: &'a str,
    pub dry_run: bool,
}

pub const LABEL_PROJECT: &str = "majnet.project";
pub const LABEL_APP: &str = "majnet.app";
pub const LABEL_CLASS: &str = "majnet.class";
pub const LABEL_COMMIT: &str = "majnet.commit";
pub const LABEL_CONFIG: &str = "majnet.config-hash";

pub fn network_name(project: &str) -> String {
    format!("proj-{project}")
}

/// Converge one app to its rendered manifest. Returns a human summary.
/// `extra_env` carries injected platform values (DB connection, §15).
pub async fn converge_app(
    ctx: &DeployCtx<'_>,
    manifest: &AppManifest,
    secrets: Option<&BTreeMap<String, String>>,
    extra_env: &[(String, String)],
) -> Result<String> {
    let config_hash = config_hash(manifest, secrets, extra_env);
    let existing = list_app_containers(ctx, &manifest.name).await?;

    let current = existing.iter().find(|c| {
        label(c, LABEL_CONFIG) == Some(config_hash.as_str())
            && c.state == Some(bollard::models::ContainerSummaryStateEnum::RUNNING)
    });
    if current.is_some() {
        return Ok("in sync".into());
    }
    if ctx.dry_run {
        return Ok(format!(
            "DRY RUN: would deploy {} ({})",
            manifest.image,
            &config_hash[..8]
        ));
    }

    pull_image(ctx.docker, &manifest.image).await?;

    // Secrets land on the node's tmpfs before anything runs.
    let secrets_dir = crate::secrets::host_dir(ctx.project, &manifest.name, ctx.class);
    if let Some(secrets) = secrets {
        crate::secrets::deliver(ctx.docker, &secrets_dir, secrets)
            .await
            .context("delivering secrets")?;
    }

    // Migrations: one-shot, must exit 0 before the rollout (§12.6).
    if let Some(migration) = &manifest.migration {
        run_migration(
            ctx,
            manifest,
            secrets.is_some(),
            &secrets_dir,
            extra_env,
            &migration.command,
        )
        .await?;
    }

    let name = format!(
        "{}-{}-{}-{}",
        ctx.project,
        manifest.name,
        ctx.class.as_str(),
        &config_hash[..8]
    );
    remove_container_if_exists(ctx.docker, &name).await?; // crashed previous attempt

    let body = container_spec(
        ctx,
        manifest,
        secrets.is_some(),
        &secrets_dir,
        extra_env,
        &config_hash,
    );
    ctx.docker
        .create_container(
            Some(qp::CreateContainerOptions {
                name: Some(name.clone()),
                ..Default::default()
            }),
            body,
        )
        .await
        .context("creating container")?;
    ctx.docker
        .start_container(&name, None::<qp::StartContainerOptions>)
        .await
        .context("starting container")?;

    // Production apps with an ingress also join the shared `edge` network so
    // edge-main (Traefik) can route to them (ADR 0007). The network is ensured
    // by the platform-services pass, which runs before projects.
    if ctx.class == EnvClass::Production && manifest.ingress.is_some() {
        // Best-effort: if the edge network isn't there yet (edge-main not
        // converged, or local/smoke mode), the app just isn't routable until a
        // later cycle — don't fail the deploy over it.
        if let Err(e) = ctx
            .docker
            .connect_network(
                "edge",
                bollard::models::NetworkConnectRequest {
                    container: name.clone(),
                    ..Default::default()
                },
            )
            .await
        {
            tracing::warn!(
                app = manifest.name,
                error = format!("{e:#}"),
                "could not attach app to the edge network (is edge-main up?)"
            );
        }
    }

    // Health gate. Failure leaves the old container serving.
    if let Err(e) = await_healthy(ctx.docker, &name, manifest).await {
        let _ = ctx
            .docker
            .remove_container(
                &name,
                Some(qp::RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;
        return Err(e.context("health check failed — old container keeps serving"));
    }

    // Drain: the new container is healthy (and routed); stop the old ones.
    for old in &existing {
        if let Some(old_name) = container_name(old) {
            if old_name != name {
                remove_container_if_exists(ctx.docker, &old_name).await?;
            }
        }
    }
    Ok(format!(
        "deployed {} ({})",
        manifest.image,
        &config_hash[..8]
    ))
}

/// Restart all running containers of one app (the §16 escape hatch —
/// same digest, same config, just a bounce). Returns how many.
pub async fn restart_app(ctx: &DeployCtx<'_>, app: &str) -> Result<usize> {
    let containers = list_app_containers(ctx, app).await?;
    let mut restarted = 0;
    for container in &containers {
        if let Some(name) = container_name(container) {
            ctx.docker
                .restart_container(&name, None::<qp::RestartContainerOptions>)
                .await
                .with_context(|| format!("restarting {name}"))?;
            restarted += 1;
        }
    }
    Ok(restarted)
}

/// Remove all containers of one app (project + class scoped).
pub async fn remove_app(ctx: &DeployCtx<'_>, app: &str) -> Result<()> {
    for container in list_app_containers(ctx, app).await? {
        if let Some(name) = container_name(&container) {
            remove_container_if_exists(ctx.docker, &name).await?;
        }
    }
    Ok(())
}

/// Distinct app names with live containers in this project/class.
pub async fn list_class_apps(ctx: &DeployCtx<'_>) -> Result<Vec<String>> {
    let mut apps: Vec<String> = list_class_containers(ctx)
        .await?
        .iter()
        .filter_map(|c| label(c, LABEL_APP).map(String::from))
        .collect();
    apps.sort();
    apps.dedup();
    Ok(apps)
}

/// Remove every majnet container of this project/class whose app is NOT in
/// the rendered set — deletions only when config is gone from git (§12).
pub async fn gc_removed_apps(ctx: &DeployCtx<'_>, rendered_apps: &[String]) -> Result<Vec<String>> {
    let all = list_class_containers(ctx).await?;
    let mut removed = Vec::new();
    for container in &all {
        let Some(app) = label(container, LABEL_APP) else {
            continue;
        };
        if rendered_apps.iter().any(|a| a == app) {
            continue;
        }
        if let Some(name) = container_name(container) {
            if ctx.dry_run {
                removed.push(format!("DRY RUN: would remove {name}"));
            } else {
                remove_container_if_exists(ctx.docker, &name).await?;
                removed.push(name);
            }
        }
    }
    Ok(removed)
}

fn config_hash(
    manifest: &AppManifest,
    secrets: Option<&BTreeMap<String, String>>,
    extra_env: &[(String, String)],
) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(serde_yaml::to_string(manifest).expect("manifest serializes"));
    for (k, v) in secrets
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .chain(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
    {
        hasher.update(k);
        hasher.update([0]);
        hasher.update(v);
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

fn env_list(manifest: &AppManifest, extra_env: &[(String, String)]) -> Vec<String> {
    manifest
        .env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .chain(extra_env.iter().map(|(k, v)| format!("{k}={v}")))
        .collect()
}

fn container_spec(
    ctx: &DeployCtx<'_>,
    manifest: &AppManifest,
    with_secrets: bool,
    secrets_dir: &str,
    extra_env: &[(String, String)],
    config_hash: &str,
) -> ContainerCreateBody {
    let mut labels = HashMap::from([
        (LABEL_PROJECT.to_string(), ctx.project.to_string()),
        (LABEL_APP.to_string(), manifest.name.clone()),
        (LABEL_CLASS.to_string(), ctx.class.as_str().to_string()),
        (LABEL_COMMIT.to_string(), ctx.commit.to_string()),
        (LABEL_CONFIG.to_string(), config_hash.to_string()),
    ]);
    if let Some(ingress) = &manifest.ingress {
        let router = format!("{}-{}-{}", ctx.project, manifest.name, ctx.class.as_str());
        labels.insert("traefik.enable".into(), "true".into());
        // OR every hostname the ingress serves (primary + custom domains,
        // possibly across several Cloudflare zones) — ADR 0007.
        let rule = ingress
            .hosts()
            .iter()
            .map(|h| format!("Host(`{h}`)"))
            .collect::<Vec<_>>()
            .join(" || ");
        labels.insert(format!("traefik.http.routers.{router}.rule"), rule);
        labels.insert(
            format!("traefik.http.routers.{router}.entrypoints"),
            "websecure".into(),
        );
        labels.insert(format!("traefik.http.routers.{router}.tls"), "true".into());
        labels.insert(
            format!("traefik.http.services.{router}.loadbalancer.server.port"),
            ingress.port.to_string(),
        );
    }

    let health = manifest.health.as_ref().map(|h| HealthConfig {
        test: Some(vec![
            "CMD-SHELL".into(),
            format!(
                "wget -q -O /dev/null http://127.0.0.1:{port}{path} || curl -fso /dev/null http://127.0.0.1:{port}{path}",
                port = h.port,
                path = h.path
            ),
        ]),
        interval: Some(5_000_000_000),      // 5 s in ns
        timeout: Some(3_000_000_000),       // 3 s
        retries: Some(h.retries as i64),
        start_period: Some(10_000_000_000), // 10 s grace
        ..Default::default()
    });

    ContainerCreateBody {
        image: Some(manifest.image.clone()),
        env: Some(env_list(manifest, extra_env)),
        labels: Some(labels),
        healthcheck: health,
        host_config: Some(HostConfig {
            network_mode: Some(network_name(ctx.project)),
            binds: with_secrets.then(|| vec![format!("{secrets_dir}:/run/secrets:ro")]),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn run_migration(
    ctx: &DeployCtx<'_>,
    manifest: &AppManifest,
    with_secrets: bool,
    secrets_dir: &str,
    extra_env: &[(String, String)],
    command: &[String],
) -> Result<()> {
    let name = format!(
        "{}-{}-{}-migrate",
        ctx.project,
        manifest.name,
        ctx.class.as_str()
    );
    remove_container_if_exists(ctx.docker, &name).await?;

    let body = ContainerCreateBody {
        image: Some(manifest.image.clone()),
        cmd: Some(command.to_vec()),
        env: Some(env_list(manifest, extra_env)),
        labels: Some(HashMap::from([(
            LABEL_PROJECT.to_string(),
            ctx.project.to_string(),
        )])),
        host_config: Some(HostConfig {
            network_mode: Some(network_name(ctx.project)),
            binds: with_secrets.then(|| vec![format!("{secrets_dir}:/run/secrets:ro")]),
            ..Default::default()
        }),
        ..Default::default()
    };
    ctx.docker
        .create_container(
            Some(qp::CreateContainerOptions {
                name: Some(name.clone()),
                ..Default::default()
            }),
            body,
        )
        .await?;
    ctx.docker
        .start_container(&name, None::<qp::StartContainerOptions>)
        .await?;

    let exit = ctx
        .docker
        .wait_container(&name, None::<qp::WaitContainerOptions>)
        .try_collect::<Vec<_>>()
        .await
        .context("waiting for migration")?
        .pop()
        .map(|r| r.status_code)
        .unwrap_or(-1);
    remove_container_if_exists(ctx.docker, &name).await?;
    if exit != 0 {
        bail!("migration exited with status {exit} — deploy aborted");
    }
    Ok(())
}

async fn await_healthy(docker: &Docker, name: &str, manifest: &AppManifest) -> Result<()> {
    use bollard::models::HealthStatusEnum;
    let Some(health) = &manifest.health else {
        // No health check defined: settle briefly, require the container to
        // still be running.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let state = docker
            .inspect_container(name, None::<qp::InspectContainerOptions>)
            .await?;
        let running = state
            .state
            .as_ref()
            .and_then(|s| s.running)
            .unwrap_or(false);
        if !running {
            bail!("container exited immediately (no health check defined)");
        }
        return Ok(());
    };

    // start_period(10s) + retries × (interval 5s + timeout 3s) + slack.
    let deadline = std::time::Duration::from_secs(15 + (health.retries as u64) * 9);
    let started = std::time::Instant::now();
    loop {
        let state = docker
            .inspect_container(name, None::<qp::InspectContainerOptions>)
            .await?;
        let status = state
            .state
            .as_ref()
            .and_then(|s| s.health.as_ref())
            .and_then(|h| h.status);
        match status {
            Some(HealthStatusEnum::HEALTHY) => return Ok(()),
            Some(HealthStatusEnum::UNHEALTHY) => bail!("container reported unhealthy"),
            _ => {}
        }
        if started.elapsed() > deadline {
            bail!("health check timed out after {deadline:?}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn pull_image(docker: &Docker, image: &str) -> Result<()> {
    if docker.inspect_image(image).await.is_ok() {
        return Ok(()); // digests are immutable — a present image is the image
    }
    // NOTE: private GHCR packages need node-level pull auth (docker login on
    // the node, done at bootstrap) — the reconciler itself holds no GitHub
    // credentials by design (§6). Tracked in roadmap open questions.
    docker
        .create_image(
            Some(qp::CreateImageOptions {
                from_image: Some(image.into()),
                ..Default::default()
            }),
            None,
            None,
        )
        .try_collect::<Vec<_>>()
        .await
        .with_context(|| format!("pulling {image}"))?;
    Ok(())
}

async fn list_app_containers(ctx: &DeployCtx<'_>, app: &str) -> Result<Vec<ContainerSummary>> {
    list_containers(
        ctx.docker,
        vec![
            format!("{LABEL_PROJECT}={}", ctx.project),
            format!("{LABEL_APP}={app}"),
            format!("{LABEL_CLASS}={}", ctx.class.as_str()),
        ],
    )
    .await
}

async fn list_class_containers(ctx: &DeployCtx<'_>) -> Result<Vec<ContainerSummary>> {
    list_containers(
        ctx.docker,
        vec![
            format!("{LABEL_PROJECT}={}", ctx.project),
            format!("{LABEL_CLASS}={}", ctx.class.as_str()),
        ],
    )
    .await
}

async fn list_containers(
    docker: &Docker,
    label_filters: Vec<String>,
) -> Result<Vec<ContainerSummary>> {
    let filters = HashMap::from([("label".to_string(), label_filters)]);
    Ok(docker
        .list_containers(Some(qp::ListContainersOptions {
            all: true,
            filters: Some(filters),
            ..Default::default()
        }))
        .await?)
}

fn label<'a>(container: &'a ContainerSummary, key: &str) -> Option<&'a str> {
    container.labels.as_ref()?.get(key).map(String::as_str)
}

fn container_name(container: &ContainerSummary) -> Option<String> {
    container
        .names
        .as_ref()?
        .first()
        .map(|n| n.trim_start_matches('/').to_string())
}

async fn remove_container_if_exists(docker: &Docker, name: &str) -> Result<()> {
    match docker
        .remove_container(
            name,
            Some(qp::RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await
    {
        Ok(()) => Ok(()),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
