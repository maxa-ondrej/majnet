//! Reconciler-owned platform services (ADR 0007). Phase 1: `edge-main`.
//!
//! The reconciler deploys the platform `edge-main` stack onto the prod node
//! over the Docker API — no SSH. Traefik's config files come from the platform
//! repo snapshot and are delivered to a host path via the same
//! helper-container + `put_archive` path as secrets; the container is
//! (re)created only when a hash of its config (image + files) changes.

use anyhow::{ensure, Context, Result};
use bollard::models::{
    ContainerCreateBody, HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters as qp;
use bollard::Docker;
use majnet_common::platform::NodesFile;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

use crate::snapshot::Snapshot;
use crate::AppState;

const EDGE_NETWORK: &str = "edge";
const EDGE_IMAGE: &str = "traefik:v3.6";
const EDGE_CONFIG_DIR: &str = "/etc/majnet/edge-main";
const ORIGIN_CERTS_DIR: &str = "/etc/majnet/origin-certs";
const HELPER_IMAGE: &str = "busybox:stable";
const LABEL_CONFIG: &str = "majnet.config";

/// Converge platform services onto their role's nodes. Non-fatal: a failure
/// logs and lets project convergence proceed. Skipped in local/smoke mode —
/// binding host 80/443 there is neither wanted nor safe.
pub async fn converge_platform(state: &AppState, nodes: &NodesFile, platform: &Snapshot) {
    if state.config.docker_local {
        return;
    }
    let Some(prod) = nodes.by_role("prod") else {
        return;
    };
    let docker = match state.nodes(nodes).client_for(prod).await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "prod Docker unavailable for edge-main");
            return;
        }
    };
    if let Err(e) = converge_edge_main(&docker, platform).await {
        tracing::error!(error = format!("{e:#}"), "edge-main convergence failed");
        let _ = state.store.record(
            &platform.commit,
            "platform",
            "prod",
            "edge-main",
            &format!("FAILED: {e:#}"),
        );
    }
}

async fn converge_edge_main(docker: &Docker, platform: &Snapshot) -> Result<()> {
    // Traefik's config from the platform repo (platform/edge-main/traefik/*).
    let prefix = "platform/edge-main/traefik/";
    let config: BTreeMap<String, Vec<u8>> = platform
        .files
        .iter()
        .filter_map(|(p, c)| p.strip_prefix(prefix).map(|rel| (rel.to_string(), c.clone())))
        .collect();
    ensure!(
        config.contains_key("traefik.yaml"),
        "platform/edge-main/traefik/traefik.yaml missing from the platform repo"
    );

    // Image + files → hash. A change (new cert, config edit, image bump) forces
    // a recreate; an unchanged, running edge-main is left alone.
    let hash = config_hash(&config);
    if running_with_hash(docker, "edge-main", &hash).await? {
        return Ok(());
    }

    ensure_network(docker, EDGE_NETWORK).await?;
    ensure_image(docker, EDGE_IMAGE).await?;
    ensure_image(docker, HELPER_IMAGE).await?;
    deliver_files(docker, EDGE_CONFIG_DIR, &config).await?;
    remove_container(docker, "edge-main").await;

    let binds = vec![
        "/var/run/docker.sock:/var/run/docker.sock:ro".to_string(),
        format!("{EDGE_CONFIG_DIR}/traefik.yaml:/etc/traefik/traefik.yaml:ro"),
        format!("{EDGE_CONFIG_DIR}/dynamic:/etc/traefik/dynamic:ro"),
        format!("{ORIGIN_CERTS_DIR}:/certs:ro"),
    ];
    let port = |p: &str| {
        (
            format!("{p}/tcp"),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".into()),
                host_port: Some(p.into()),
            }]),
        )
    };
    let created = docker
        .create_container(
            Some(qp::CreateContainerOptions {
                name: Some("edge-main".into()),
                ..Default::default()
            }),
            ContainerCreateBody {
                image: Some(EDGE_IMAGE.into()),
                labels: Some(HashMap::from([(LABEL_CONFIG.to_string(), hash)])),
                exposed_ports: Some(vec!["80/tcp".into(), "443/tcp".into()]),
                host_config: Some(HostConfig {
                    network_mode: Some(EDGE_NETWORK.into()),
                    binds: Some(binds),
                    port_bindings: Some(HashMap::from([port("80"), port("443")])),
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
        .context("creating edge-main")?;
    docker
        .start_container(&created.id, None::<qp::StartContainerOptions>)
        .await
        .context("starting edge-main")?;
    tracing::info!(commit = %platform.commit, "edge-main deployed");
    Ok(())
}

fn config_hash(config: &BTreeMap<String, Vec<u8>>) -> String {
    let mut h = Sha256::new();
    h.update(EDGE_IMAGE.as_bytes());
    for (path, content) in config {
        h.update(path.as_bytes());
        h.update([0]);
        h.update(content);
        h.update([0]);
    }
    hex::encode(h.finalize())[..16].to_string()
}

/// True if a container named `name` is running with a matching config-hash
/// label (nothing to do).
async fn running_with_hash(docker: &Docker, name: &str, hash: &str) -> Result<bool> {
    let filters = HashMap::from([("name".to_string(), vec![name.to_string()])]);
    let containers = docker
        .list_containers(Some(qp::ListContainersOptions {
            all: true,
            filters: Some(filters),
            ..Default::default()
        }))
        .await
        .context("listing edge-main")?;
    Ok(containers.iter().any(|c| {
        c.state == Some(bollard::models::ContainerSummaryStateEnum::RUNNING)
            && c.labels
                .as_ref()
                .and_then(|l| l.get(LABEL_CONFIG))
                .map(String::as_str)
                == Some(hash)
    }))
}

async fn ensure_network(docker: &Docker, name: &str) -> Result<()> {
    if docker
        .inspect_network(name, None::<qp::InspectNetworkOptions>)
        .await
        .is_ok()
    {
        return Ok(());
    }
    docker
        .create_network(bollard::models::NetworkCreateRequest {
            name: name.into(),
            ..Default::default()
        })
        .await
        .with_context(|| format!("creating network {name}"))?;
    Ok(())
}

async fn ensure_image(docker: &Docker, image: &str) -> Result<()> {
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

/// Deliver `files` (relative paths, may include subdirs) into host `dir` on the
/// node via a short-lived helper container (same mechanism as secrets).
async fn deliver_files(docker: &Docker, dir: &str, files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let helper = docker
        .create_container(
            None::<qp::CreateContainerOptions>,
            ContainerCreateBody {
                image: Some(HELPER_IMAGE.into()),
                cmd: Some(vec!["sleep".into(), "60".into()]),
                host_config: Some(HostConfig {
                    binds: Some(vec![format!("{dir}:/dest")]),
                    auto_remove: Some(true),
                    ..Default::default()
                }),
                labels: Some(HashMap::from([(
                    "majnet.helper".to_string(),
                    "platform".to_string(),
                )])),
                ..Default::default()
            },
        )
        .await
        .context("creating file-delivery helper")?;

    let result = async {
        docker
            .start_container(&helper.id, None::<qp::StartContainerOptions>)
            .await?;
        docker
            .upload_to_container(
                &helper.id,
                Some(qp::UploadToContainerOptions {
                    path: "/dest".into(),
                    ..Default::default()
                }),
                bollard::body_full(tar_of(files)?.into()),
            )
            .await
            .context("uploading config archive")?;
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let _ = docker
        .kill_container(&helper.id, None::<qp::KillContainerOptions>)
        .await;
    result
}

fn tar_of(files: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, content) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o444);
        header.set_cksum();
        builder.append_data(&mut header, name, content.as_slice())?;
    }
    Ok(builder.into_inner()?)
}

async fn remove_container(docker: &Docker, name: &str) {
    let _ = docker
        .remove_container(
            name,
            Some(qp::RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
}
