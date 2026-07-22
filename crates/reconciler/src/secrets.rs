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

/// Decrypt an inline `secrets:` map (ADR 0024) — each value is
/// `majnet:<base64(age ciphertext)>` — with the class key. Returns the plaintext
/// name→value map. Bails on the first malformed envelope or decrypt failure — no
/// partial applies (§12). Uses the `age` binary directly (no SOPS envelope).
pub async fn decrypt_inline(
    config: &Config,
    class: EnvClass,
    inline: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    use base64::Engine;
    let key_file = config
        .age_key_dir
        .join(format!("age-{}.key", class.as_str()));
    ensure!(
        key_file.exists(),
        "missing class age key {}",
        key_file.display()
    );

    let mut out = BTreeMap::new();
    for (key, value) in inline {
        let body = value
            .strip_prefix(majnet_common::manifest::SECRET_ENVELOPE_PREFIX)
            .with_context(|| format!("secret '{key}' is not a majnet: envelope"))?;
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(body)
            .with_context(|| format!("secret '{key}' has an invalid base64 body"))?;
        let plaintext = age_decrypt(&key_file, &ciphertext)
            .await
            .with_context(|| format!("decrypting secret '{key}'"))?;
        out.insert(key.clone(), plaintext);
    }
    Ok(out)
}

/// Decrypt raw `age` ciphertext with a specific class key file, returning the
/// plaintext as a UTF-8 string.
async fn age_decrypt(key_file: &std::path::Path, ciphertext: &[u8]) -> Result<String> {
    let mut child = tokio::process::Command::new("age")
        .args(["-d", "-i", key_file.to_str().context("non-utf8 key path")?])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning age (is it installed?)")?;
    child
        .stdin
        .take()
        .context("no stdin")?
        .write_all(ciphertext)
        .await?;
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "age decrypt failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8(out.stdout).context("decrypted secret is not valid UTF-8")
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
