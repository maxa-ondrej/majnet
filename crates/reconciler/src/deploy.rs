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
    ContainerCreateBody, ContainerSummary, EndpointSettings, HealthConfig, HostConfig,
    NetworkingConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
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
    /// For fetching a GHCR pull credential from the bot (ADR 0012).
    pub http: &'a reqwest::Client,
    pub bot_url: &'a str,
    /// This class's node's WireGuard mesh IP. Used to publish an app's
    /// `wg_ports` on the WG tunnel so cross-node peers can reach them. Empty for
    /// imperative paths that never create containers (purge/rename/restart) and
    /// for local smoke tests (no WG mesh) — an empty IP publishes nothing.
    pub wireguard_ip: &'a str,
}

pub const LABEL_PROJECT: &str = "majnet.project";
pub const LABEL_APP: &str = "majnet.app";
pub const LABEL_CLASS: &str = "majnet.class";
pub const LABEL_COMMIT: &str = "majnet.commit";
pub const LABEL_CONFIG: &str = "majnet.config-hash";

/// Salt folded into `config_hash`. Bump when container-spec *generation* changes
/// in a way not reflected in the manifest/secrets inputs (e.g. a new derived
/// label) so running apps re-converge onto the fresh spec via a normal
/// blue-green rollout instead of silently keeping a stale one.
/// History: "2" — added the Traefik LB healthcheck labels.
///          "3" — added the stable intra-project network alias (ADR 0019).
const SPEC_VERSION: &str = "3";

pub fn network_name(project: &str) -> String {
    format!("proj-{project}")
}

/// The Docker named volume backing a manifest volume on the app's node.
/// Deterministic per (project, app, class, name) so blue-green redeploys reuse
/// the same volume — data persists.
pub fn volume_name(project: &str, app: &str, class: EnvClass, name: &str) -> String {
    format!("majnet-{project}-{app}-{}-{}", class.as_str(), name)
}

/// The container bind mounts: the secrets tmpfs (read-only, if any) plus each
/// declared persistent volume. `None` when there's nothing to bind.
fn mount_binds(
    ctx: &DeployCtx<'_>,
    manifest: &AppManifest,
    with_secrets: bool,
    secrets_dir: &str,
) -> Option<Vec<String>> {
    let mut binds = Vec::new();
    if with_secrets {
        binds.push(format!("{secrets_dir}:/run/secrets:ro"));
    }
    for v in &manifest.volumes {
        let vol = volume_name(ctx.project, &manifest.name, ctx.class, &v.name);
        binds.push(format!("{vol}:{}", v.path));
    }
    (!binds.is_empty()).then_some(binds)
}

/// Create the app's persistent volumes on the node (idempotent — Docker returns
/// the existing volume for a name that's already there). Never deleted on
/// teardown: data is preserved ("archive, never delete").
async fn ensure_volumes(ctx: &DeployCtx<'_>, manifest: &AppManifest) -> Result<()> {
    for v in &manifest.volumes {
        let name = volume_name(ctx.project, &manifest.name, ctx.class, &v.name);
        if ctx.dry_run {
            tracing::info!(volume = name, "DRY RUN: would ensure volume");
            continue;
        }
        ctx.docker
            .create_volume(bollard::models::VolumeCreateRequest {
                name: Some(name.clone()),
                labels: Some(HashMap::from([
                    (LABEL_PROJECT.to_string(), ctx.project.to_string()),
                    (LABEL_APP.to_string(), manifest.name.clone()),
                    (LABEL_CLASS.to_string(), ctx.class.as_str().to_string()),
                ])),
                ..Default::default()
            })
            .await
            .with_context(|| format!("creating volume {name}"))?;
    }
    Ok(())
}

/// Converge one app to its rendered manifest. Returns a human summary.
/// `extra_env` carries injected platform values (DB connection, §15).
pub async fn converge_app(
    ctx: &DeployCtx<'_>,
    manifest: &AppManifest,
    secrets: Option<&BTreeMap<String, String>>,
    extra_env: &[(String, String)],
    tracker: &crate::state::DeployTracker<'_>,
) -> Result<String> {
    let config_hash = config_hash(manifest, secrets, extra_env, ctx.wireguard_ip);
    let replicas = manifest.replicas.max(1);
    let base = format!(
        "{}-{}-{}-{}",
        ctx.project,
        manifest.name,
        ctx.class.as_str(),
        &config_hash[..8]
    );
    // Replica 1 keeps the unsuffixed name so existing single-container apps stay
    // in sync (no churn); further replicas get a `-N` suffix.
    let desired: Vec<String> = (1..=replicas)
        .map(|i| {
            if i == 1 {
                base.clone()
            } else {
                format!("{base}-{i}")
            }
        })
        .collect();
    let desired_set: std::collections::HashSet<&str> = desired.iter().map(String::as_str).collect();

    let existing = list_app_containers(ctx, &manifest.name).await?;
    let running_current: std::collections::HashSet<String> = existing
        .iter()
        .filter(|c| {
            label(c, LABEL_CONFIG) == Some(config_hash.as_str())
                && c.state == Some(bollard::models::ContainerSummaryStateEnum::RUNNING)
        })
        .filter_map(container_name)
        .collect();
    // In sync when every desired replica runs at the current hash and no other
    // (old-hash or surplus) container of this app is left.
    let in_sync = desired.iter().all(|n| running_current.contains(n))
        && existing
            .iter()
            .filter_map(container_name)
            .all(|n| desired_set.contains(n.as_str()));
    if in_sync {
        return Ok("in sync".into());
    }
    if ctx.dry_run {
        return Ok(format!(
            "DRY RUN: would deploy {} ({}, {replicas} replica{})",
            manifest.image_ref(),
            &config_hash[..8],
            if replicas == 1 { "" } else { "s" }
        ));
    }

    // Past the in-sync/dry-run short-circuits: this is an actual rollout, so
    // start tracking its stages (deploy trackability). Resolve the effective
    // image reference (combined `image` or bare-repo + `digest`/`tag`) once.
    let image_ref = manifest.image_ref();
    tracker.stage("pulling", &image_ref);
    pull_image(ctx, &image_ref).await?;

    // Secrets land on the node's tmpfs before anything runs.
    let secrets_dir = crate::secrets::host_dir(ctx.project, &manifest.name, ctx.class);
    if let Some(secrets) = secrets {
        crate::secrets::deliver(ctx.docker, &secrets_dir, secrets)
            .await
            .context("delivering secrets")?;
    }

    // Persistent volumes exist before the container mounts them.
    ensure_volumes(ctx, manifest).await?;

    // Migrations: one-shot, must exit 0 before the rollout (§12.6). Runs in its
    // own image when the manifest gives one (ADR 0009), else the app image.
    if let Some(migration) = &manifest.migration {
        tracker.stage("migrating", migration.image(&image_ref));
        run_migration(
            ctx,
            manifest,
            secrets.is_some(),
            &secrets_dir,
            extra_env,
            migration.image(&image_ref),
            &migration.command,
        )
        .await?;
    }

    tracker.stage(
        "starting",
        &format!("{replicas} replica{}", if replicas == 1 { "" } else { "s" }),
    );
    // Create each missing replica, health-gating before moving on. A failure
    // removes just that replica and aborts — the old set keeps serving (nothing
    // is drained until all desired replicas are healthy).
    for name in &desired {
        if running_current.contains(name) {
            continue;
        }
        remove_container_if_exists(ctx.docker, name).await?; // crashed previous attempt
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
            .start_container(name, None::<qp::StartContainerOptions>)
            .await
            .context("starting container")?;

        // Production apps with an ingress also join the shared `edge` network so
        // edge-main (Traefik) can route to (and load-balance across) them
        // (ADR 0007). Best-effort — not routable until edge-main is up.
        if ctx.class == EnvClass::Production && manifest.ingress.is_some() {
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

        // Health gate. Failure leaves the old set serving.
        tracker.stage("health", name);
        if let Err(e) = await_healthy(ctx.docker, name, manifest).await {
            let _ = ctx
                .docker
                .remove_container(
                    name,
                    Some(qp::RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await;
            return Err(e.context("health check failed — old container keeps serving"));
        }
    }

    // Drain: the new replicas are healthy (and routed); remove old-hash
    // containers and any surplus replicas (scale-down).
    tracker.stage("finalizing", "routing + draining previous generation");
    for old in &existing {
        if let Some(old_name) = container_name(old) {
            if !desired_set.contains(old_name.as_str()) {
                remove_container_if_exists(ctx.docker, &old_name).await?;
            }
        }
    }
    Ok(format!(
        "deployed {} ({}, {replicas} replica{})",
        image_ref,
        &config_hash[..8],
        if replicas == 1 { "" } else { "s" }
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
    wireguard_ip: &str,
) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(SPEC_VERSION);
    hasher.update([0]);
    // Replica count is not part of the identity — scaling up/down adds/removes
    // containers rather than recreating them — so normalize it out of the hash.
    let mut normalized = manifest.clone();
    normalized.replicas = 1;
    hasher.update(serde_yaml::to_string(&normalized).expect("manifest serializes"));
    // When the app publishes `wg_ports`, the node's WG IP is part of the spec
    // (it's the published host IP) but not of the manifest — fold it in so a
    // node re-address re-converges. Only when wg_ports is set, so apps that
    // don't use the mesh keep a WG-IP-independent hash (no fleet re-converge).
    if !manifest.wg_ports.is_empty() {
        hasher.update(b"wg\0");
        hasher.update(wireguard_ip.as_bytes());
        hasher.update([0]);
    }
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
    // A host-less ingress (a production app that declared only a port, since
    // ADR 0013 made `host` optional) has nothing to route to — skip the Traefik
    // labels rather than emit an empty `Host()` rule. Non-production classes
    // always have an auto-assigned host by this point.
    if let Some(ingress) = manifest.ingress.as_ref().filter(|i| !i.hosts().is_empty()) {
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
        // Load-balancer healthcheck → true zero-downtime rollout: during the
        // blue-green overlap Traefik only routes to backends passing this
        // check, so the still-starting new container gets no traffic until it's
        // healthy (and the old one is drained only after `await_healthy`).
        if let Some(health) = &manifest.health {
            let svc = format!("traefik.http.services.{router}.loadbalancer.healthcheck");
            labels.insert(format!("{svc}.path"), health.path.clone());
            labels.insert(format!("{svc}.port"), health.port.to_string());
            labels.insert(format!("{svc}.interval"), "3s".into());
            labels.insert(format!("{svc}.timeout"), "2s".into());
        }
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

    // Optional resource limits (manifest validated these already; treat any
    // parse slip as unlimited rather than failing the deploy).
    let (memory, nano_cpus) = manifest
        .resources
        .as_ref()
        .map(|r| {
            (
                r.memory_bytes().ok().flatten(),
                r.nano_cpus().ok().flatten(),
            )
        })
        .unwrap_or((None, None));

    // Stable intra-project DNS alias (ADR 0019): sibling apps on the same
    // project network resolve this app by its manifest name (e.g. an app's own
    // reverse proxy → `sideline-server`), independent of the volatile
    // `<project>-<app>-<class>-<hash>` container name that blue-green churns.
    // The alias key must match `network_mode` for Docker to accept both.
    let net = network_name(ctx.project);
    let networking_config = NetworkingConfig {
        endpoints_config: Some(HashMap::from([(
            net.clone(),
            EndpointSettings {
                aliases: Some(vec![manifest.name.clone()]),
                ..Default::default()
            },
        )])),
    };

    // Cross-node mesh endpoints (ADR 0023): publish each `wg_ports` entry on the
    // node's WireGuard IP only — reachable fleet-wide over the WG tunnel, never
    // on a public interface (mirrors the Adminer host-IP binding). Skipped when
    // there's no WG IP (local smoke tests).
    let (exposed_ports, port_bindings) = wg_port_bindings(&manifest.wg_ports, ctx.wireguard_ip);

    ContainerCreateBody {
        image: Some(manifest.image_ref()),
        env: Some(env_list(manifest, extra_env)),
        labels: Some(labels),
        healthcheck: health,
        networking_config: Some(networking_config),
        exposed_ports,
        host_config: Some(HostConfig {
            network_mode: Some(net),
            binds: mount_binds(ctx, manifest, with_secrets, secrets_dir),
            port_bindings,
            memory,
            nano_cpus,
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build the `exposed_ports` + `port_bindings` that publish an app's `wg_ports`
/// on the node's WireGuard IP (`<wg_ip>:<port>` → `<port>/tcp`). Returns
/// `(None, None)` when there are no ports or no WG IP, so non-mesh apps and
/// local smoke tests publish nothing.
#[allow(clippy::type_complexity)]
fn wg_port_bindings(
    wg_ports: &[u16],
    wireguard_ip: &str,
) -> (
    Option<Vec<String>>,
    Option<HashMap<String, Option<Vec<PortBinding>>>>,
) {
    if wg_ports.is_empty() || wireguard_ip.is_empty() {
        return (None, None);
    }
    let exposed = wg_ports.iter().map(|p| format!("{p}/tcp")).collect();
    let bindings = wg_ports
        .iter()
        .map(|p| {
            (
                format!("{p}/tcp"),
                Some(vec![PortBinding {
                    host_ip: Some(wireguard_ip.to_string()),
                    host_port: Some(p.to_string()),
                }]),
            )
        })
        .collect();
    (Some(exposed), Some(bindings))
}

async fn run_migration(
    ctx: &DeployCtx<'_>,
    manifest: &AppManifest,
    with_secrets: bool,
    secrets_dir: &str,
    extra_env: &[(String, String)],
    image: &str,
    command: &[String],
) -> Result<()> {
    let name = format!(
        "{}-{}-{}-migrate",
        ctx.project,
        manifest.name,
        ctx.class.as_str()
    );
    remove_container_if_exists(ctx.docker, &name).await?;

    // The app image was already pulled; a distinct migration image needs its own.
    if image != manifest.image_ref() {
        pull_image(ctx, image).await?;
    }

    let body = ContainerCreateBody {
        image: Some(image.to_string()),
        cmd: Some(command.to_vec()),
        env: Some(env_list(manifest, extra_env)),
        labels: Some(HashMap::from([(
            LABEL_PROJECT.to_string(),
            ctx.project.to_string(),
        )])),
        host_config: Some(HostConfig {
            network_mode: Some(network_name(ctx.project)),
            binds: mount_binds(ctx, manifest, with_secrets, secrets_dir),
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

async fn pull_image(ctx: &DeployCtx<'_>, image: &str) -> Result<()> {
    if ctx.docker.inspect_image(image).await.is_ok() {
        return Ok(()); // digests are immutable — a present image is the image
    }
    // Private GHCR app images need pull auth. The reconciler holds no GitHub
    // credentials by design (§6), so it fetches a short-lived GHCR credential
    // from the bot (which holds the App + `packages: read`) — ADR 0012.
    let credentials = ghcr_credentials(ctx, image).await;
    ctx.docker
        .create_image(
            Some(qp::CreateImageOptions {
                from_image: Some(image.into()),
                ..Default::default()
            }),
            None,
            credentials,
        )
        .try_collect::<Vec<_>>()
        .await
        .with_context(|| format!("pulling {image}"))?;
    Ok(())
}

#[derive(serde::Deserialize)]
struct RegistryAuth {
    username: String,
    password: String,
}

/// A GHCR pull credential from the bot for `ghcr.io/<org>/…` images (ADR 0012).
/// `None` for non-GHCR images (public registries need no auth) or if the bot is
/// unreachable — the pull then proceeds unauthenticated (fine for public images).
async fn ghcr_credentials(
    ctx: &DeployCtx<'_>,
    image: &str,
) -> Option<bollard::auth::DockerCredentials> {
    let org = image.strip_prefix("ghcr.io/")?.split('/').next()?;
    let url = format!("{}/api/registry-auth/{}", ctx.bot_url, org);
    match ctx
        .http
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(resp) => match resp.json::<RegistryAuth>().await {
            Ok(auth) => Some(bollard::auth::DockerCredentials {
                username: Some(auth.username),
                password: Some(auth.password),
                ..Default::default()
            }),
            Err(e) => {
                tracing::warn!(image, error = %e, "registry-auth parse failed; pulling unauthenticated");
                None
            }
        },
        Err(e) => {
            tracing::warn!(image, error = %e, "registry-auth fetch failed; pulling unauthenticated");
            None
        }
    }
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

pub(crate) async fn remove_container_if_exists(docker: &Docker, name: &str) -> Result<()> {
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

/// Remove the project's Docker network, tolerating "already gone" (404). Only
/// reached by the whole-project purge (§2 escape), after every container on it
/// has been removed.
pub async fn remove_network(docker: &Docker, project: &str) -> Result<()> {
    let name = network_name(project);
    match docker.remove_network(&name).await {
        Ok(()) => Ok(()),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing network {name}")),
    }
}

/// Force-remove a named volume, tolerating "already gone" (404). The one place
/// MajNet deletes data — only reached by the archived-app purge (§2 escape).
pub async fn remove_volume(docker: &Docker, name: &str) -> Result<()> {
    match docker
        .remove_volume(name, Some(qp::RemoveVolumeOptions { force: true }))
        .await
    {
        Ok(()) => Ok(()),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing volume {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(wg_ports: Vec<u16>) -> AppManifest {
        AppManifest::parse(&format!(
            "name: api\nimage: ghcr.io/org/api@sha256:{}\nwg_ports: {wg_ports:?}\n",
            "a".repeat(64)
        ))
        .unwrap()
    }

    #[test]
    fn wg_port_bindings_publish_on_the_wireguard_ip_only() {
        let (exposed, bindings) = wg_port_bindings(&[4317, 4318], "10.88.0.3");
        assert_eq!(
            exposed.unwrap(),
            vec!["4317/tcp".to_string(), "4318/tcp".to_string()]
        );
        let b = bindings.unwrap();
        let one = b["4317/tcp"].as_ref().unwrap();
        assert_eq!(one[0].host_ip.as_deref(), Some("10.88.0.3"));
        assert_eq!(one[0].host_port.as_deref(), Some("4317"));
    }

    #[test]
    fn wg_port_bindings_publish_nothing_without_ports_or_ip() {
        // No WG IP (local smoke test) → nothing published even with ports.
        assert!(wg_port_bindings(&[4317], "").0.is_none());
        assert!(wg_port_bindings(&[4317], "").1.is_none());
        // No ports → nothing published.
        assert!(wg_port_bindings(&[], "10.88.0.3").0.is_none());
    }

    #[test]
    fn config_hash_folds_wg_ip_only_when_wg_ports_set() {
        // With no wg_ports, the WG IP is not part of the identity — same hash
        // on any node, so no spurious fleet-wide re-converge.
        let plain = manifest(vec![]);
        assert_eq!(
            config_hash(&plain, None, &[], "10.88.0.2"),
            config_hash(&plain, None, &[], "10.88.0.3"),
        );
        // With wg_ports, a different node IP re-converges (the published host IP
        // is part of the spec).
        let mesh = manifest(vec![4317]);
        assert_ne!(
            config_hash(&mesh, None, &[], "10.88.0.2"),
            config_hash(&mesh, None, &[], "10.88.0.3"),
        );
    }
}
