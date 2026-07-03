//! Per-project ingress on the private node (§4, §7): a Traefik instance
//! sharing a network namespace with a tailscale sidecar, so the project's
//! stable + ephemeral apps are reachable only through that project's tailnet
//! node (`tag:proj-<name>`, enforced by the bot-rendered ACLs).
//!
//!   tailscale sidecar (owns netns, joins tailnet, state in a volume)
//!   └── traefik (network_mode: container:<sidecar>)
//!       docker provider, constrained to `majnet.project=<name>` labels,
//!       attached to the project network via the sidecar
//!
//! The auth key is minted by the bot (`POST /api/tailscale-authkey/<project>`)
//! only when the sidecar is first created — tailscale state persists in a
//! named volume across restarts, so keys are one-shot and short-lived.

use anyhow::{Context, Result};
use bollard::models::{
    ContainerCreateBody, DeviceMapping, HostConfig, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters as qp;
use bollard::Docker;
use std::collections::HashMap;

use crate::deploy::{network_name, LABEL_PROJECT};
use crate::AppState;

const TAILSCALE_IMAGE: &str = "tailscale/tailscale:stable";
const TRAEFIK_IMAGE: &str = "traefik:v3.5";

pub async fn ensure_ingress(state: &AppState, docker: &Docker, project: &str) -> Result<()> {
    let sidecar = format!("proj-{project}-tailscale");
    let traefik = format!("proj-{project}-ingress");

    let sidecar_running = container_running(docker, &sidecar).await?;
    let traefik_running = container_running(docker, &traefik).await?;
    if sidecar_running && traefik_running {
        return Ok(());
    }
    if state.config.dry_run {
        tracing::info!(project, "DRY RUN: would (re)create ingress stack");
        return Ok(());
    }
    tracing::info!(
        project,
        "creating ingress stack (traefik + tailscale sidecar)"
    );

    if !sidecar_running {
        // One-shot key from the bot; state volume keeps identity thereafter.
        let authkey = state
            .http
            .post(format!(
                "{}/api/tailscale-authkey/{project}",
                state.config.bot_url
            ))
            .send()
            .await?
            .error_for_status()
            .context("bot refused to mint a tailscale auth key")?
            .text()
            .await?;

        remove_if_exists(docker, &sidecar).await?;
        pull(docker, TAILSCALE_IMAGE).await?;
        docker
            .create_container(
                Some(qp::CreateContainerOptions {
                    name: Some(sidecar.clone()),
                    ..Default::default()
                }),
                ContainerCreateBody {
                    image: Some(TAILSCALE_IMAGE.into()),
                    hostname: Some(project.to_string()), // tailnet name = project
                    env: Some(vec![
                        format!("TS_AUTHKEY={authkey}"),
                        "TS_STATE_DIR=/var/lib/tailscale".into(),
                        "TS_USERSPACE=false".into(),
                    ]),
                    labels: Some(HashMap::from([(
                        LABEL_PROJECT.to_string(),
                        project.to_string(),
                    )])),
                    host_config: Some(HostConfig {
                        network_mode: Some(network_name(project)),
                        binds: Some(vec![format!("proj-{project}-ts-state:/var/lib/tailscale")]),
                        cap_add: Some(vec!["NET_ADMIN".into()]),
                        devices: Some(vec![DeviceMapping {
                            path_on_host: Some("/dev/net/tun".into()),
                            path_in_container: Some("/dev/net/tun".into()),
                            cgroup_permissions: Some("rwm".into()),
                        }]),
                        restart_policy: Some(RestartPolicy {
                            name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .context("creating tailscale sidecar")?;
        docker
            .start_container(&sidecar, None::<qp::StartContainerOptions>)
            .await?;
    }

    if !traefik_running {
        remove_if_exists(docker, &traefik).await?;
        pull(docker, TRAEFIK_IMAGE).await?;
        docker
            .create_container(
                Some(qp::CreateContainerOptions {
                    name: Some(traefik.clone()),
                    ..Default::default()
                }),
                ContainerCreateBody {
                    image: Some(TRAEFIK_IMAGE.into()),
                    cmd: Some(vec![
                        "--providers.docker=true".into(),
                        "--providers.docker.exposedbydefault=false".into(),
                        // Only this project's containers — cross-project
                        // isolation even if a manifest lies about its host.
                        format!(
                            "--providers.docker.constraints=Label(`{LABEL_PROJECT}`,`{project}`)"
                        ),
                        "--entrypoints.web.address=:80".into(),
                        "--entrypoints.websecure.address=:443".into(),
                    ]),
                    labels: Some(HashMap::from([(
                        LABEL_PROJECT.to_string(),
                        project.to_string(),
                    )])),
                    host_config: Some(HostConfig {
                        // Shares the sidecar's netns: listens on the tailnet IP.
                        network_mode: Some(format!("container:{sidecar}")),
                        binds: Some(vec!["/var/run/docker.sock:/var/run/docker.sock:ro".into()]),
                        restart_policy: Some(RestartPolicy {
                            name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .context("creating ingress traefik")?;
        docker
            .start_container(&traefik, None::<qp::StartContainerOptions>)
            .await?;
    }
    Ok(())
}

async fn container_running(docker: &Docker, name: &str) -> Result<bool> {
    match docker
        .inspect_container(name, None::<qp::InspectContainerOptions>)
        .await
    {
        Ok(c) => Ok(c.state.as_ref().and_then(|s| s.running).unwrap_or(false)),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

async fn remove_if_exists(docker: &Docker, name: &str) -> Result<()> {
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
        Ok(())
        | Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn pull(docker: &Docker, image: &str) -> Result<()> {
    use futures_util::TryStreamExt;
    if docker.inspect_image(image).await.is_ok() {
        return Ok(());
    }
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
