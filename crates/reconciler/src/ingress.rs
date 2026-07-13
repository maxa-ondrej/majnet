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
//!
//! Traefik serves `websecure:443` with a browser-trusted wildcard cert
//! `*.<project>.<base_domain>` (ADR 0013 phase 3): the bot commits the cert
//! (key age-encrypted) to the platform repo, and here the reconciler decrypts
//! it and delivers it + a file-provider default-cert config to the ingress —
//! recreating Traefik when the cert changes. Without a committed cert Traefik
//! still comes up (Traefik's self-signed default), just untrusted.

use anyhow::{Context, Result};
use bollard::models::{
    ContainerCreateBody, DeviceMapping, HostConfig, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters as qp;
use bollard::Docker;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use crate::deploy::{network_name, LABEL_CONFIG, LABEL_PROJECT};
use crate::snapshot::Snapshot;
use crate::AppState;

const TAILSCALE_IMAGE: &str = "tailscale/tailscale:stable";
const TRAEFIK_IMAGE: &str = "traefik:v3.5";
/// Per-project host dir holding the wildcard cert + Traefik dynamic config.
const INGRESS_STATE_DIR: &str = "/etc/majnet/ingress";

pub async fn ensure_ingress(
    state: &AppState,
    docker: &Docker,
    project: &str,
    platform: &Snapshot,
) -> Result<()> {
    let sidecar = format!("proj-{project}-tailscale");
    let traefik = format!("proj-{project}-ingress");

    // The wildcard cert the bot committed (ADR 0013), if any. Its content feeds
    // the Traefik config hash, so a renewed cert forces a Traefik recreate.
    let cert = load_wildcard_cert(platform, &state.config.age_key_dir, project)
        .await
        .with_context(|| format!("loading ingress cert for {project}"))?;
    let hash = ingress_hash(cert.as_ref());

    let sidecar_running = container_running(docker, &sidecar).await?;
    // Traefik shares the sidecar's netns, so it is only healthy if the sidecar
    // is up; requiring `sidecar_running` here also forces a Traefik recreate
    // whenever the sidecar is (re)created.
    let traefik_ok = sidecar_running && running_with_hash(docker, &traefik, &hash).await?;
    if sidecar_running && traefik_ok {
        return Ok(());
    }
    if state.config.dry_run {
        tracing::info!(project, "DRY RUN: would (re)create ingress stack");
        return Ok(());
    }
    tracing::info!(
        project,
        has_cert = cert.is_some(),
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

    if !traefik_ok {
        let mut cmd = vec![
            "--providers.docker=true".to_string(),
            "--providers.docker.exposedbydefault=false".to_string(),
            // Only this project's containers — cross-project isolation even if
            // a manifest lies about its host.
            format!("--providers.docker.constraints=Label(`{LABEL_PROJECT}`,`{project}`)"),
            "--entrypoints.web.address=:80".to_string(),
            "--entrypoints.websecure.address=:443".to_string(),
        ];
        let mut binds = vec!["/var/run/docker.sock:/var/run/docker.sock:ro".to_string()];

        // With a committed wildcard cert, deliver it + a file-provider dynamic
        // config that makes it Traefik's default certificate, so every
        // `websecure` router serves browser-trusted TLS with no per-app work.
        if let Some(files) = &cert {
            let certs_dir = format!("{INGRESS_STATE_DIR}/{project}/certs");
            let dyn_dir = format!("{INGRESS_STATE_DIR}/{project}/dynamic");
            crate::platform::deliver_files(docker, &certs_dir, files).await?;
            crate::platform::deliver_files(docker, &dyn_dir, &dynamic_tls_config()).await?;
            cmd.push("--providers.file.directory=/etc/traefik/dynamic".into());
            cmd.push("--providers.file.watch=true".into());
            binds.push(format!("{certs_dir}:/certs:ro"));
            binds.push(format!("{dyn_dir}:/etc/traefik/dynamic:ro"));
        }

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
                    cmd: Some(cmd),
                    labels: Some(HashMap::from([
                        (LABEL_PROJECT.to_string(), project.to_string()),
                        (LABEL_CONFIG.to_string(), hash),
                    ])),
                    host_config: Some(HostConfig {
                        // Shares the sidecar's netns: listens on the tailnet IP.
                        network_mode: Some(format!("container:{sidecar}")),
                        binds: Some(binds),
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

/// Traefik dynamic config making the delivered wildcard the default cert for
/// every `websecure` router (no per-app SNI entries needed).
fn dynamic_tls_config() -> BTreeMap<String, Vec<u8>> {
    let yaml = "tls:\n  stores:\n    default:\n      defaultCertificate:\n        \
                certFile: /certs/wildcard.crt\n        keyFile: /certs/wildcard.key\n";
    BTreeMap::from([("majnet-tls.yaml".to_string(), yaml.as_bytes().to_vec())])
}

/// The wildcard cert + decrypted key for `project`, or `None` if the bot hasn't
/// committed one yet (ADR 0013 phase 2). Returns the file set delivered into the
/// ingress `/certs` mount: `wildcard.crt` + `wildcard.key`.
async fn load_wildcard_cert(
    platform: &Snapshot,
    age_key_dir: &Path,
    project: &str,
) -> Result<Option<BTreeMap<String, Vec<u8>>>> {
    let crt_path = format!("platform/ingress-certs/{project}.crt");
    let key_path = format!("platform/ingress-certs/{project}.key.age");
    let (Some(crt), Some(key_enc)) =
        (platform.files.get(&crt_path), platform.files.get(&key_path))
    else {
        return Ok(None);
    };
    let key = crate::platform::age_decrypt(age_key_dir, key_enc).await?;
    Ok(Some(BTreeMap::from([
        ("wildcard.crt".to_string(), crt.clone()),
        ("wildcard.key".to_string(), key.into_bytes()),
    ])))
}

/// Config hash for the ingress Traefik: image + the delivered cert bytes, so a
/// renewed cert (or first issuance) changes the hash and forces a recreate.
fn ingress_hash(cert: Option<&BTreeMap<String, Vec<u8>>>) -> String {
    let mut h = Sha256::new();
    h.update(TRAEFIK_IMAGE.as_bytes());
    if let Some(files) = cert {
        for (name, content) in files {
            h.update(name.as_bytes());
            h.update([0]);
            h.update(content);
            h.update([0]);
        }
    }
    hex::encode(h.finalize())[..16].to_string()
}

/// True iff `name` is running with a matching config-hash label.
async fn running_with_hash(docker: &Docker, name: &str, hash: &str) -> Result<bool> {
    match docker
        .inspect_container(name, None::<qp::InspectContainerOptions>)
        .await
    {
        Ok(c) => {
            let running = c.state.as_ref().and_then(|s| s.running).unwrap_or(false);
            let label_ok = c
                .config
                .and_then(|cfg| cfg.labels)
                .and_then(|l| l.get(LABEL_CONFIG).cloned())
                .as_deref()
                == Some(hash);
            Ok(running && label_ok)
        }
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(false),
        Err(e) => Err(e.into()),
    }
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
