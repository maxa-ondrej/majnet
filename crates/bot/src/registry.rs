//! Minimal GHCR (OCI registry v2) image copy — used by app rename to move a
//! digest-pinned image from the old package to the new one so the manifest pin
//! stays valid after the repo (and thus the CI image name) changes.
//!
//! Renaming a GitHub repo does NOT move existing GHCR packages, so a rename that
//! rewrites `ghcr.io/<org>/<old>@<digest>` → `…/<new>@<digest>` would otherwise
//! leave a pin the reconciler can't pull. We copy the digest across packages via
//! same-registry blob mounts (cheap — no blob bytes move) + a manifest re-PUT
//! under the identical digest (content-addressed, so the digest is preserved).
//!
//! Credentials: the same GHCR token `registry_auth` hands the reconciler, so a
//! successful copy needs a token with `write:packages` (the configured PAT).

use anyhow::{bail, Context, Result};
use serde::Deserialize;

const REGISTRY: &str = "https://ghcr.io";
const ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json,\
application/vnd.docker.distribution.manifest.list.v2+json,\
application/vnd.oci.image.manifest.v1+json,\
application/vnd.oci.image.index.v1+json";

/// Copy `digest` from `ghcr.io/<org>/<from>` to `ghcr.io/<org>/<to>`, including
/// any child manifests (multi-arch) and all referenced blobs. Idempotent: a
/// digest already present in the target is re-PUT harmlessly.
pub async fn copy_image(
    http: &reqwest::Client,
    org: &str,
    from: &str,
    to: &str,
    digest: &str,
    username: &str,
    password: &str,
) -> Result<()> {
    let token = auth_token(http, org, from, to, username, password)
        .await
        .context("obtaining GHCR registry token")?;
    copy_digest(http, org, from, to, digest, &token).await
}

#[derive(Deserialize)]
struct TokenResp {
    #[serde(alias = "access_token")]
    token: String,
}

/// Resolve a tag to a fully-qualified digest-pinned image ref
/// (`ghcr.io/<org>/<name>@sha256:…`) via the registry v2 manifest API. Used by
/// the control-plane update surface to learn the digest of the latest CI build.
/// `name` is the full package name after the org (e.g. `majnet/control-plane`).
pub async fn resolve_digest(
    http: &reqwest::Client,
    org: &str,
    name: &str,
    tag: &str,
    username: &str,
    password: &str,
) -> Result<String> {
    let token = pull_token(http, org, name, username, password)
        .await
        .context("obtaining GHCR pull token")?;
    // A HEAD would suffice, but GHCR is more reliable on GET; we only read the
    // Docker-Content-Digest header, not the body.
    let resp = http
        .get(manifest_url(org, name, tag))
        .bearer_auth(&token)
        .header("Accept", ACCEPT)
        .send()
        .await?;
    let status = resp.status();
    let digest = resp
        .headers()
        .get("docker-content-digest")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if !status.is_success() {
        bail!(
            "resolving {org}/{name}:{tag} ({status}): {}",
            resp.text().await.unwrap_or_default()
        );
    }
    let digest = digest.context("registry response missing Docker-Content-Digest")?;
    Ok(format!("ghcr.io/{org}/{name}@{digest}"))
}

/// A bearer token scoped to pull `org/name`.
async fn pull_token(
    http: &reqwest::Client,
    org: &str,
    name: &str,
    username: &str,
    password: &str,
) -> Result<String> {
    let url = format!("{REGISTRY}/token?service=ghcr.io&scope=repository:{org}/{name}:pull");
    let resp = http
        .get(&url)
        .basic_auth(username, Some(password))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "GHCR token request failed ({status}): {body}"
    );
    Ok(serde_json::from_str::<TokenResp>(&body)
        .context("parsing GHCR token response")?
        .token)
}

/// A bearer token scoped to pull `from` and push+pull `to` within the org.
async fn auth_token(
    http: &reqwest::Client,
    org: &str,
    from: &str,
    to: &str,
    username: &str,
    password: &str,
) -> Result<String> {
    let url = format!(
        "{REGISTRY}/token?service=ghcr.io\
         &scope=repository:{org}/{from}:pull\
         &scope=repository:{org}/{to}:push,pull"
    );
    let resp = http
        .get(&url)
        .basic_auth(username, Some(password))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "GHCR token request failed ({status}): {body}"
    );
    Ok(serde_json::from_str::<TokenResp>(&body)
        .context("parsing GHCR token response")?
        .token)
}

#[derive(Deserialize)]
struct Descriptor {
    digest: String,
}

#[derive(Deserialize)]
struct Manifest {
    // Present on image manifests.
    config: Option<Descriptor>,
    #[serde(default)]
    layers: Vec<Descriptor>,
    // Present on manifest lists / OCI indexes.
    #[serde(default)]
    manifests: Vec<Descriptor>,
}

fn manifest_url(org: &str, repo: &str, reference: &str) -> String {
    format!("{REGISTRY}/v2/{org}/{repo}/manifests/{reference}")
}

/// Recursively copy one manifest (image or index) + its blobs.
async fn copy_digest(
    http: &reqwest::Client,
    org: &str,
    from: &str,
    to: &str,
    digest: &str,
    token: &str,
) -> Result<()> {
    // Fetch the raw manifest bytes — must be re-PUT verbatim so the digest is
    // preserved (any re-serialization would change the content hash).
    let resp = http
        .get(manifest_url(org, from, digest))
        .bearer_auth(token)
        .header("Accept", ACCEPT)
        .send()
        .await?;
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/vnd.oci.image.manifest.v1+json")
        .to_string();
    let bytes = resp.bytes().await?;
    anyhow::ensure!(
        status.is_success(),
        "fetching manifest {digest} from {org}/{from} ({status}): {}",
        String::from_utf8_lossy(&bytes)
    );

    let parsed: Manifest = serde_json::from_slice(&bytes).context("parsing manifest JSON")?;

    if !parsed.manifests.is_empty() {
        // Manifest list / index: copy each child image first, then the index.
        for child in &parsed.manifests {
            Box::pin(copy_digest(http, org, from, to, &child.digest, token)).await?;
        }
    } else {
        // Image manifest: mount the config blob + every layer.
        if let Some(config) = &parsed.config {
            mount_blob(http, org, from, to, &config.digest, token).await?;
        }
        for layer in &parsed.layers {
            mount_blob(http, org, from, to, &layer.digest, token).await?;
        }
    }

    // Register the manifest under the same digest in the target repo.
    let put = http
        .put(manifest_url(org, to, digest))
        .bearer_auth(token)
        .header("Content-Type", content_type)
        .body(bytes)
        .send()
        .await?;
    let put_status = put.status();
    anyhow::ensure!(
        put_status.is_success(),
        "PUT manifest {digest} to {org}/{to} ({put_status}): {}",
        put.text().await.unwrap_or_default()
    );
    Ok(())
}

/// Cross-repo mount a blob within the same registry (no bytes transferred). Falls
/// back to a monolithic upload if the registry declines the mount.
async fn mount_blob(
    http: &reqwest::Client,
    org: &str,
    from: &str,
    to: &str,
    digest: &str,
    token: &str,
) -> Result<()> {
    let mount = http
        .post(format!(
            "{REGISTRY}/v2/{org}/{to}/blobs/uploads/?mount={digest}&from={org}/{from}"
        ))
        .bearer_auth(token)
        .header("content-length", "0")
        .send()
        .await?;
    match mount.status().as_u16() {
        201 => Ok(()), // mounted
        202 => {
            // Mount declined — an upload session was opened instead. Stream the
            // blob bytes from the source and complete the upload.
            let location = mount
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .context("blob upload session has no Location")?
                .to_string();
            let blob = http
                .get(format!("{REGISTRY}/v2/{org}/{from}/blobs/{digest}"))
                .bearer_auth(token)
                .send()
                .await?;
            anyhow::ensure!(
                blob.status().is_success(),
                "fetching blob {digest} from {org}/{from}: {}",
                blob.status()
            );
            let body = blob.bytes().await?;
            let sep = if location.contains('?') { '&' } else { '?' };
            let put = http
                .put(format!("{location}{sep}digest={digest}"))
                .bearer_auth(token)
                .header("Content-Type", "application/octet-stream")
                .body(body)
                .send()
                .await?;
            anyhow::ensure!(
                put.status().is_success(),
                "uploading blob {digest} to {org}/{to}: {}",
                put.status()
            );
            Ok(())
        }
        other => bail!(
            "mounting blob {digest} into {org}/{to} failed ({other}): {}",
            mount.text().await.unwrap_or_default()
        ),
    }
}
