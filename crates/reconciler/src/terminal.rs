//! Interactive node terminal (ADR 0016) — a WebSocket the dashboard opens to
//! get a live shell, bridged to a bollard `exec` over the node's Docker mTLS
//! connection. Two modes, one duplex path:
//!
//!   - **container** — `exec` into a running *app* container (app containers
//!     only; control-plane + DB engines are not addressable here).
//!   - **host** — start a pinned, `--privileged --pid=host` helper container and
//!     `exec` `nsenter -t 1 …` into it → a root shell in the host namespaces.
//!     Reuses the reconciler's Docker credential; no SSH, no new credential.
//!
//! Platform-admin only and *named* (the header-less WG `Infra` bypass is refused
//! — every session is attributable). The full session is recorded: a TTY echoes
//! typed input, so the output stream captured to `data_dir/transcripts/<id>.log`
//! is the whole transcript. Open/close are also audit events.

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use bollard::models::{ContainerCreateBody, ExecConfig, HostConfig};
use bollard::Docker;
use futures_util::{SinkExt, StreamExt};
use majnet_common::platform::{NodesFile, ProjectsFile};
use serde::Deserialize;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

use crate::AppState;

type ApiError = (StatusCode, String);

#[derive(Debug, Deserialize)]
pub struct TermQuery {
    /// `host` | `container`.
    pub mode: String,
    /// Host mode: the node name.
    pub node: Option<String>,
    /// Container mode: the project org, app, and env class.
    pub project: Option<String>,
    pub app: Option<String>,
    pub class: Option<String>,
}

/// `GET /api/terminal` (WebSocket upgrade) — platform-admin, named human only.
pub async fn terminal_ws(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TermQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, (StatusCode, String)> {
    let actor = crate::authz::require_named_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    Ok(ws.on_upgrade(move |socket| async move {
        if let Err(e) = run_session(state, q, actor, socket).await {
            tracing::warn!(error = %format!("{e:#}"), "terminal session error");
        }
    }))
}

/// `GET /api/terminal/sessions` (platform-admin) — recent recorded sessions.
pub async fn sessions_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<crate::state::TerminalSession>>, ApiError> {
    crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    state
        .store
        .terminal_sessions(200)
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
}

/// `GET /api/terminal/transcript/{id}` (platform-admin) — the full recorded I/O
/// for a session (raw PTY output; the caller strips ANSI for display).
pub async fn transcript_get(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<String, ApiError> {
    crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let path = state
        .config
        .data_dir
        .join("transcripts")
        .join(format!("{id}.log"));
    match tokio::fs::read(&path).await {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err((
            StatusCode::NOT_FOUND,
            "no transcript for this session".into(),
        )),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))),
    }
}

/// What we resolved a request into: the node's Docker client + the container to
/// exec in, plus (host mode) a helper container to reap on close.
struct Target {
    docker: Docker,
    container: String,
    /// Command to run in the exec.
    cmd: Vec<String>,
    /// Helper container to remove when the session ends (host mode).
    helper: Option<String>,
}

async fn run_session(
    state: Arc<AppState>,
    q: TermQuery,
    actor: String,
    socket: WebSocket,
) -> Result<()> {
    // Open the audit row first so the transcript filename + helper name are
    // stable, and the session is on record even if setup then fails.
    let (mode, node_hint, target_label, audit_project) = describe(&q)?;
    let session_id = state
        .store
        .terminal_open(&actor, &node_hint, &mode, &target_label)?;
    state.store.record(
        "terminal",
        &audit_project,
        &node_hint,
        &format!("terminal open {mode} {target_label}"),
        &format!("by {actor}"),
    )?;
    tracing::info!(%actor, %mode, target = %target_label, session_id, "terminal session opened");

    let (mut socket, result) = match resolve(&state, &q, session_id).await {
        Ok(t) => bridge(&state, socket, t, session_id).await,
        Err(e) => {
            // Report the failure into the terminal, then close.
            let mut s = socket;
            let _ = s
                .send(Message::Binary(
                    format!("\r\n\x1b[31mterminal error: {e:#}\x1b[0m\r\n")
                        .into_bytes()
                        .into(),
                ))
                .await;
            (s, Err(e))
        }
    };

    let bytes = result.as_ref().copied().unwrap_or(0);
    state.store.terminal_close(session_id, bytes)?;
    state.store.record(
        "terminal",
        &audit_project,
        &node_hint,
        &format!("terminal close {mode} {target_label}"),
        &format!("by {actor} · {bytes} bytes"),
    )?;
    tracing::info!(%actor, session_id, bytes, "terminal session closed");
    let _ = socket.send(Message::Close(None)).await;
    result.map(|_| ())
}

/// (mode, node-name-hint, target-label, audit-project) without touching Docker —
/// so the audit row can open before setup.
fn describe(q: &TermQuery) -> Result<(String, String, String, String)> {
    match q.mode.as_str() {
        "host" => {
            let node = q.node.clone().context("host mode requires a node")?;
            Ok(("host".into(), node.clone(), node.clone(), node))
        }
        "container" => {
            let project = q
                .project
                .clone()
                .context("container mode requires a project")?;
            let app = q.app.clone().context("container mode requires an app")?;
            let class = q.class.clone().context("container mode requires a class")?;
            Ok((
                "container".into(),
                String::new(),
                format!("{project}/{app} ({class})"),
                project,
            ))
        }
        other => anyhow::bail!("unknown terminal mode: {other}"),
    }
}

async fn resolve(state: &AppState, q: &TermQuery, session_id: i64) -> Result<Target> {
    let platform = crate::snapshot::fetch(
        &state.http,
        &state.config,
        &state.config.root_org,
        "platform",
        "main",
    )
    .await?
    .context("platform snapshot unavailable")?;
    let nodes = NodesFile::parse(platform.files.get("nodes.yaml").context("no nodes.yaml")?)?;

    match q.mode.as_str() {
        "host" => {
            let name = q.node.clone().context("host mode requires a node")?;
            let node = nodes
                .nodes
                .iter()
                .find(|n| n.name == name)
                .with_context(|| format!("unknown node {name}"))?;
            let docker = state.nodes(&nodes).client_for(node).await?;
            let helper = format!("majnet-term-{session_id}");
            start_host_helper(&docker, &state.config.term_helper_image, &helper).await?;
            Ok(Target {
                docker,
                container: helper.clone(),
                cmd: vec![
                    "nsenter",
                    "-t",
                    "1",
                    "-m",
                    "-u",
                    "-i",
                    "-n",
                    "-p",
                    "--",
                    "/bin/bash",
                    "-il",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                helper: Some(helper),
            })
        }
        "container" => {
            let project = q
                .project
                .clone()
                .context("container mode requires a project")?;
            let app = q.app.clone().context("container mode requires an app")?;
            let class_str = q.class.clone().context("container mode requires a class")?;
            let class: majnet_common::EnvClass =
                serde_yaml::from_str(&class_str).map_err(|_| {
                    anyhow::anyhow!("class must be production|stable|testing|ephemeral")
                })?;
            let node = nodes
                .by_role(class.node_role())
                .context("no node for class")?;
            let docker = state.nodes(&nodes).client_for(node).await?;
            let container =
                find_app_container(&docker, &platform, &project, &app, class.as_str()).await?;
            Ok(Target {
                docker,
                container,
                // Prefer bash, fall back to sh — app images vary. Detect bash by
                // PATH lookup (not `exec bash 2>/dev/null`, which would send the
                // *shell's own* stderr to /dev/null → non-interactive, no prompt,
                // errors swallowed). `-i` forces an interactive prompt.
                cmd: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "if command -v bash >/dev/null 2>&1; then exec bash -i; else exec sh -i 2>/dev/null || exec sh; fi".into(),
                ],
                helper: None,
            })
        }
        other => anyhow::bail!("unknown terminal mode: {other}"),
    }
}

/// Start a `--privileged --pid=host` helper running `sleep infinity`, so we can
/// `exec nsenter` into the host namespaces through it. The image is pulled on
/// demand if the node doesn't have it yet (the default `debian:bookworm-slim` is
/// public; pin by digest in production).
async fn start_host_helper(docker: &Docker, image: &str, name: &str) -> Result<()> {
    use bollard::query_parameters::{CreateContainerOptionsBuilder, StartContainerOptions};
    ensure_helper_image(docker, image).await?;
    let options = CreateContainerOptionsBuilder::default().name(name).build();
    let config = ContainerCreateBody {
        image: Some(image.to_string()),
        cmd: Some(vec!["sleep".into(), "infinity".into()]),
        host_config: Some(HostConfig {
            privileged: Some(true),
            pid_mode: Some("host".into()),
            auto_remove: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    docker
        .create_container(Some(options), config)
        .await
        .context("creating host-shell helper (is the helper image pulled on the node?)")?;
    docker
        .start_container(name, None::<StartContainerOptions>)
        .await
        .context("starting host-shell helper")?;
    Ok(())
}

/// Pull the host-shell helper image if the node doesn't already have it. The
/// default (`debian:bookworm-slim`) is public, so no registry auth is needed —
/// unlike app images (ADR 0012), the reconciler pulls this one directly.
async fn ensure_helper_image(docker: &Docker, image: &str) -> Result<()> {
    use bollard::query_parameters::CreateImageOptions;
    use futures_util::TryStreamExt;
    if docker.inspect_image(image).await.is_ok() {
        return Ok(());
    }
    docker
        .create_image(
            Some(CreateImageOptions {
                from_image: Some(image.to_string()),
                ..Default::default()
            }),
            None,
            None,
        )
        .try_collect::<Vec<_>>()
        .await
        .with_context(|| format!("pulling host-shell helper image {image}"))?;
    Ok(())
}

/// The running container id for an app/class, matched by the deploy labels (same
/// resolution logs/restart use — the project org maps to the project name).
async fn find_app_container(
    docker: &Docker,
    platform: &crate::snapshot::Snapshot,
    org: &str,
    app: &str,
    class: &str,
) -> Result<String> {
    let proj_name = platform
        .files
        .get("projects.yaml")
        .and_then(|b| serde_yaml::from_slice::<ProjectsFile>(b).ok())
        .and_then(|pf| pf.projects.into_iter().find(|p| p.org == org))
        .map(|p| p.name)
        .unwrap_or_else(|| org.to_string());
    let filters = std::collections::HashMap::from([(
        "label".to_string(),
        vec![
            format!("{}={}", crate::deploy::LABEL_PROJECT, proj_name),
            format!("{}={}", crate::deploy::LABEL_APP, app),
            format!("{}={}", crate::deploy::LABEL_CLASS, class),
        ],
    )]);
    let list = docker
        .list_containers(Some(bollard::query_parameters::ListContainersOptions {
            all: false, // running only
            filters: Some(filters),
            ..Default::default()
        }))
        .await?;
    list.into_iter()
        .find_map(|c| c.id)
        .context("no running container for this app/class")
}

/// Bridge the WebSocket to a bollard exec: exec output → WS (+ transcript file),
/// WS binary → exec stdin, WS text → resize. Returns the bytes recorded.
async fn bridge(
    state: &AppState,
    socket: WebSocket,
    target: Target,
    session_id: i64,
) -> (WebSocket, Result<u64>) {
    let (socket, r) = bridge_inner(state, socket, &target, session_id).await;
    // Best-effort reap of the host-shell helper.
    if let Some(helper) = &target.helper {
        use bollard::query_parameters::RemoveContainerOptionsBuilder;
        let _ = target
            .docker
            .remove_container(
                helper,
                Some(RemoveContainerOptionsBuilder::default().force(true).build()),
            )
            .await;
    }
    (socket, r)
}

async fn bridge_inner(
    state: &AppState,
    socket: WebSocket,
    target: &Target,
    session_id: i64,
) -> (WebSocket, Result<u64>) {
    let exec = match create_exec(target).await {
        Ok(e) => e,
        Err(e) => return (socket, Err(e)),
    };
    let (exec_id, mut output, mut input) = exec;

    // Transcript file (best-effort; a failure here shouldn't kill the session).
    let dir = state.config.data_dir.join("transcripts");
    let _ = tokio::fs::create_dir_all(&dir).await;
    let mut file = tokio::fs::File::create(dir.join(format!("{session_id}.log")))
        .await
        .ok();

    let (mut ws_tx, mut ws_rx) = socket.split();
    let mut bytes: u64 = 0;
    let err: Result<()> = loop {
        tokio::select! {
            out = output.next() => match out {
                Some(Ok(log)) => {
                    let b = log.into_bytes();
                    bytes += b.len() as u64;
                    if let Some(f) = file.as_mut() { let _ = f.write_all(&b).await; }
                    if ws_tx.send(Message::Binary(b)).await.is_err() { break Ok(()); }
                }
                Some(Err(e)) => break Err(anyhow::Error::new(e).context("exec output")),
                None => break Ok(()),
            },
            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if input.write_all(&b).await.is_err() { break Ok(()); }
                    let _ = input.flush().await;
                }
                Some(Ok(Message::Text(t))) => {
                    if let Some((cols, rows)) = parse_resize(t.as_str()) {
                        let _ = target
                            .docker
                            .resize_exec(&exec_id, bollard::exec::ResizeExecOptions { height: rows, width: cols })
                            .await;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break Ok(()),
                Some(Ok(_)) => {} // ping/pong handled by axum
                Some(Err(_)) => break Ok(()),
            },
        }
    };
    if let Some(f) = file.as_mut() {
        let _ = f.flush().await;
    }
    let socket = ws_tx.reunite(ws_rx).expect("split halves belong together");
    (socket, err.map(|_| bytes))
}

type ExecDuplex = (
    String,
    std::pin::Pin<
        Box<
            dyn futures_util::Stream<
                    Item = Result<bollard::container::LogOutput, bollard::errors::Error>,
                > + Send,
        >,
    >,
    std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
);

async fn create_exec(target: &Target) -> Result<ExecDuplex> {
    let exec = target
        .docker
        .create_exec(
            &target.container,
            ExecConfig {
                cmd: Some(target.cmd.clone()),
                attach_stdin: Some(true),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                tty: Some(true),
                env: Some(vec!["TERM=xterm-256color".into()]),
                console_size: Some(vec![24, 80]),
                ..Default::default()
            },
        )
        .await
        .context("creating exec")?;
    match target
        .docker
        .start_exec(
            &exec.id,
            Some(bollard::exec::StartExecOptions {
                detach: false,
                tty: true,
                output_capacity: None,
            }),
        )
        .await
        .context("starting exec")?
    {
        bollard::exec::StartExecResults::Attached { output, input } => Ok((exec.id, output, input)),
        bollard::exec::StartExecResults::Detached => {
            anyhow::bail!("exec started detached — cannot attach a terminal")
        }
    }
}

/// Parse a `{"resize":{"cols":C,"rows":R}}` control message → (cols, rows).
fn parse_resize(text: &str) -> Option<(u16, u16)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let r = v.get("resize")?;
    let cols = r.get("cols")?.as_u64()? as u16;
    let rows = r.get("rows")?.as_u64()? as u16;
    Some((cols, rows))
}

#[cfg(test)]
mod tests {
    use super::parse_resize;

    #[test]
    fn parses_resize_control() {
        assert_eq!(
            parse_resize(r#"{"resize":{"cols":120,"rows":40}}"#),
            Some((120, 40))
        );
        assert_eq!(parse_resize(r#"{"data":"x"}"#), None);
        assert_eq!(parse_resize("not json"), None);
    }
}
