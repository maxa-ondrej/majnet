//! Standard app `/info` endpoint scrape (build metadata: version, commit,
//! build time — whatever the app chooses to report as JSON).
//!
//! The reconciler is the caller: right after the blue-green health gate proves
//! the new container serves HTTP, `capture` probes `/info` over a `docker exec`
//! into that container (`127.0.0.1:<port>/info`, same host+port the health check
//! already passed on) and upserts the result into the state DB keyed by
//! (project, app, class). The dashboard reads the recorded state — no probing
//! containers on every page load, and "the reconciler owns node truth" stays
//! intact.
//!
//! Best-effort throughout: a container with no shell (scratch/distroless), no
//! `/info` route, or non-JSON output records an `error` rather than failing the
//! deploy — exactly the same wget/curl-in-image assumption the health check
//! already makes (§12).

use anyhow::{Context, Result};
use bollard::Docker;
use majnet_common::manifest::AppManifest;

use crate::deploy::{DeployCtx, LABEL_APP, LABEL_CLASS, LABEL_PROJECT};
use crate::AppState;

/// Probe the freshly-deployed app's `/info`, record what it reports (or why it
/// couldn't be read) for this env, and return the reported `version` (used to
/// label the deploy event). Never returns an error — a failed probe is stored,
/// not propagated.
pub async fn capture(
    state: &AppState,
    ctx: &DeployCtx<'_>,
    manifest: &AppManifest,
) -> Option<String> {
    let (value, error) = match probe(ctx, manifest).await {
        Ok(value) => (Some(value), None),
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    let mut version = value
        .as_ref()
        .and_then(|v| v.get("version"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let mut info = value.as_ref().map(serde_json::Value::to_string);
    // Fallback for images with no MajNet `/info` route — notably services
    // running an off-the-shelf image: read the OCI `org.opencontainers.image
    // .version` label off the pulled image so the dashboard shows a real
    // version (e.g. `v1.0.0-beta34`) instead of a bare digest (ADR 0021).
    let mut error = error;
    if version.is_none() {
        if let Some(v) = image_oci_version(ctx.docker, &manifest.image).await {
            info = Some(serde_json::json!({ "version": v, "source": "image" }).to_string());
            version = Some(v);
            error = None;
        }
    }
    if let Err(e) = state.store.record_app_info(
        ctx.project,
        &manifest.name,
        ctx.class.as_str(),
        ctx.commit,
        info.as_deref(),
        error.as_deref(),
    ) {
        tracing::warn!(
            project = ctx.project,
            app = manifest.name,
            class = ctx.class.as_str(),
            error = format!("{e:#}"),
            "recording /info failed"
        );
    }
    version
}

/// Read the OCI `org.opencontainers.image.version` label off a pulled image —
/// the version fallback for images with no `/info` route. The image is local
/// (just deployed), so this is a cheap inspect. Best-effort → `None`.
async fn image_oci_version(docker: &Docker, image: &str) -> Option<String> {
    let inspect = docker.inspect_image(image).await.ok()?;
    inspect
        .config?
        .labels?
        .get("org.opencontainers.image.version")
        .cloned()
        .filter(|v| !v.is_empty())
}

/// GET `/info` from inside the running app container and parse it as JSON.
async fn probe(ctx: &DeployCtx<'_>, manifest: &AppManifest) -> Result<serde_json::Value> {
    // `/info` is a sibling of `/healthz` on the app's HTTP port. Prefer the
    // health port (already proven reachable by the gate) then the ingress port.
    let port = manifest
        .health
        .as_ref()
        .map(|h| h.port)
        .or_else(|| manifest.ingress.as_ref().map(|i| i.port))
        .context("app declares no HTTP port to probe /info on")?;

    let container = running_container(ctx, &manifest.name)
        .await?
        .context("no running container for this app/class")?;

    let body = exec_scrape(ctx.docker, &container, port).await?;
    let body = body.trim();
    anyhow::ensure!(!body.is_empty(), "no /info response (endpoint missing?)");
    serde_json::from_str(body).context("/info did not return JSON")
}

/// The id of the running container for `(ctx.project, app, ctx.class)`, if any —
/// matched by the deploy labels (same lookup metrics/logs use).
async fn running_container(ctx: &DeployCtx<'_>, app: &str) -> Result<Option<String>> {
    let filters = std::collections::HashMap::from([(
        "label".to_string(),
        vec![
            format!("{LABEL_PROJECT}={}", ctx.project),
            format!("{LABEL_APP}={app}"),
            format!("{LABEL_CLASS}={}", ctx.class.as_str()),
        ],
    )]);
    let list = ctx
        .docker
        .list_containers(Some(bollard::query_parameters::ListContainersOptions {
            all: false, // running only
            filters: Some(filters),
            ..Default::default()
        }))
        .await?;
    Ok(list.into_iter().find_map(|c| c.id))
}

/// Run wget/curl inside the container to fetch `/info` on the loopback. Mirrors
/// the health check's `wget … || curl …` fallback so it works on any image that
/// can already be health-checked. Returns captured stdout.
async fn exec_scrape(docker: &Docker, container: &str, port: u16) -> Result<String> {
    use futures_util::StreamExt;
    let script = format!(
        "wget -q -T 3 -O - http://127.0.0.1:{port}/info 2>/dev/null || \
         curl -fs -m 3 http://127.0.0.1:{port}/info"
    );
    let exec = docker
        .create_exec(
            container,
            bollard::models::ExecConfig {
                cmd: Some(vec!["sh".into(), "-c".into(), script]),
                attach_stdout: Some(true),
                attach_stderr: Some(false),
                ..Default::default()
            },
        )
        .await
        .context("starting /info probe exec (no shell in image?)")?;

    let mut out = String::new();
    if let bollard::exec::StartExecResults::Attached {
        output: mut stream, ..
    } = docker
        .start_exec(&exec.id, None::<bollard::exec::StartExecOptions>)
        .await?
    {
        while let Some(chunk) = stream.next().await {
            out.push_str(&chunk?.to_string());
        }
    }
    Ok(out)
}
