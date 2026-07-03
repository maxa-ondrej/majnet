//! SOPS + age secret delivery (§14).
//!
//! Decrypt with the class key (`age-<class>.key`) by shelling out to the
//! `sops` binary — the reconciler node has it installed; reimplementing SOPS
//! is not worth the risk. Decrypted values exist only:
//!   1. in this process's memory, and
//!   2. under `/run/majnet/secrets/<stack>/` on the target node — `/run` is
//!      tmpfs on Debian, so they never touch disk. Never env vars.
//!
//! Because the reconciler drives remote Docker APIs, it can't write node
//! files directly: delivery runs a short-lived helper container that bind-
//! mounts the tmpfs path, receives a tar stream via put_archive, and is
//! removed. App containers then bind-mount the same path read-only.

use anyhow::{bail, ensure, Context, Result};
use bollard::query_parameters as qp;
use bollard::Docker;
use majnet_common::EnvClass;
use std::collections::BTreeMap;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

use crate::config::Config;

pub const HELPER_IMAGE: &str = "busybox:stable";

/// Decrypt a rendered `secrets/<app>.yaml` (SOPS document: flat map of
/// name → value) with the class key.
pub async fn decrypt(
    config: &Config,
    class: EnvClass,
    encrypted: &[u8],
) -> Result<BTreeMap<String, String>> {
    let key_file = config
        .age_key_dir
        .join(format!("age-{}.key", class.as_str()));
    ensure!(
        key_file.exists(),
        "missing class age key {}",
        key_file.display()
    );

    let mut child = tokio::process::Command::new("sops")
        .args([
            "--decrypt",
            "--input-type",
            "yaml",
            "--output-type",
            "yaml",
            "/dev/stdin",
        ])
        .env("SOPS_AGE_KEY_FILE", &key_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning sops (is it installed?)")?;
    child
        .stdin
        .take()
        .context("no stdin")?
        .write_all(encrypted)
        .await?;
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        // Failed decrypt aborts that app loudly — no partial applies (§12).
        bail!(
            "sops decrypt failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let map: BTreeMap<String, String> = serde_yaml::from_slice(&output.stdout)
        .context("decrypted secrets are not a flat name→value map")?;
    Ok(map)
}

/// Host path where a stack's secrets live (tmpfs on Debian).
pub fn host_dir(project: &str, app: &str, class: EnvClass) -> String {
    format!("/run/majnet/secrets/{project}-{app}-{}", class.as_str())
}

/// Write `secrets` to `host_dir` on the node behind `docker`.
pub async fn deliver(docker: &Docker, dir: &str, secrets: &BTreeMap<String, String>) -> Result<()> {
    ensure_helper_image(docker).await?;

    let helper = docker
        .create_container(
            None::<qp::CreateContainerOptions>,
            bollard::models::ContainerCreateBody {
                image: Some(HELPER_IMAGE.into()),
                cmd: Some(vec!["sleep".into(), "60".into()]),
                host_config: Some(bollard::models::HostConfig {
                    binds: Some(vec![format!("{dir}:/secrets")]),
                    auto_remove: Some(true),
                    ..Default::default()
                }),
                labels: Some([("majnet.helper".to_string(), "secrets".to_string())].into()),
                ..Default::default()
            },
        )
        .await
        .context("creating secrets helper container")?;

    let result = async {
        docker
            .start_container(&helper.id, None::<qp::StartContainerOptions>)
            .await?;
        let tarball = tar_of(secrets)?;
        docker
            .upload_to_container(
                &helper.id,
                Some(qp::UploadToContainerOptions {
                    path: "/secrets".into(),
                    ..Default::default()
                }),
                bollard::body_full(tarball.into()),
            )
            .await
            .context("uploading secrets archive")?;
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Best-effort teardown either way; auto_remove cleans up after the kill.
    let _ = docker
        .kill_container(&helper.id, None::<qp::KillContainerOptions>)
        .await;
    result
}

async fn ensure_helper_image(docker: &Docker) -> Result<()> {
    use futures_util::TryStreamExt;
    if docker.inspect_image(HELPER_IMAGE).await.is_ok() {
        return Ok(());
    }
    docker
        .create_image(
            Some(qp::CreateImageOptions {
                from_image: Some(HELPER_IMAGE.into()),
                ..Default::default()
            }),
            None,
            None,
        )
        .try_collect::<Vec<_>>()
        .await
        .context("pulling helper image")?;
    Ok(())
}

/// Tar archive of secret files, mode 0400.
fn tar_of(secrets: &BTreeMap<String, String>) -> Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, value) in secrets {
        let bytes = value.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o400);
        header.set_cksum();
        builder.append_data(&mut header, name, bytes)?;
    }
    Ok(builder.into_inner()?)
}
