//! Reconciler-owned platform services (ADR 0007). Phase 1: `edge-main`.
//!
//! The reconciler deploys the platform `edge-main` stack onto the prod node
//! over the Docker API — no SSH. Traefik's config files come from the platform
//! repo snapshot and are delivered to a host path via the same
//! helper-container + `put_archive` path as secrets; the container is
//! (re)created only when a hash of its config (image + files) changes.

use anyhow::{ensure, Context, Result};
use bollard::models::{
    ContainerCreateBody, ExecConfig, HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters as qp;
use bollard::Docker;
use majnet_common::manifest::DbEngine;
use majnet_common::platform::NodesFile;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use crate::config::Config;
use crate::snapshot::Snapshot;
use crate::AppState;

const EDGE_NETWORK: &str = "edge";
const EDGE_IMAGE: &str = "traefik:v3.6";
const EDGE_CONFIG_DIR: &str = "/etc/majnet/edge-main";
const ORIGIN_CERTS_DIR: &str = "/etc/majnet/origin-certs";
const HELPER_IMAGE: &str = "busybox:stable";
const LABEL_CONFIG: &str = "majnet.config";

// Resource caps for the reconciler-managed platform containers (same
// HostConfig knobs as per-app limits, ADR — resources). Folded into each
// container's config-hash so changing a cap forces a recreate. edge-main
// (Traefik) is light; DB engines get real headroom for cache + connections.
const MB: i64 = 1024 * 1024;
const EDGE_MEM: i64 = 256 * MB;
const EDGE_NANO_CPUS: i64 = 500_000_000; // 0.5 CPU

// Managed Adminer (ADR 0014): a reconciler-owned DB browser on the prod node,
// on a private network shared with postgres — never on the public `edge`
// network (DB access stays off the public edge). Reachable over the tailnet:
// the browser port is published on the prod node's WireGuard IP only (not a
// public interface), and the main node's tailnet Caddy reverse-proxies
// `adminer.<zone>` → `<prod-wireguard-ip>:8081` over WireGuard.
const ADMINER_IMAGE: &str = "adminer:5";
const ADMINER_NAME: &str = "majnet-adminer";
const ADMIN_NETWORK: &str = "majnet-admin";
// The browser listens on 8080 in-container; the Caddy tailnet route dials 8081
// on the prod node's WireGuard IP (kept off any public interface).
const ADMINER_CONTAINER_PORT: &str = "8080";
const ADMINER_HOST_PORT: &str = "8081";
const ADMINER_MEM: i64 = 256 * MB;
const ADMINER_NANO_CPUS: i64 = 500_000_000; // 0.5 CPU

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
            tracing::error!(
                error = format!("{e:#}"),
                "prod Docker unavailable for edge-main"
            );
            return;
        }
    };
    if let Err(e) = converge_edge_main(&docker, platform, &state.config.age_key_dir).await {
        tracing::error!(error = format!("{e:#}"), "edge-main convergence failed");
        let _ = state.store.record(
            &platform.commit,
            "platform",
            "prod",
            "edge-main",
            &format!("FAILED: {e:#}"),
        );
    }
    if let Err(e) = converge_adminer(&docker, &prod.wireguard_ip).await {
        tracing::error!(error = format!("{e:#}"), "adminer convergence failed");
        let _ = state.store.record(
            &platform.commit,
            "platform",
            "prod",
            "adminer",
            &format!("FAILED: {e:#}"),
        );
    }
}

/// Managed Adminer (ADR 0014): a DB browser on a private network shared with
/// postgres, capped, config-hash-managed like edge-main. Idempotent; recreated
/// only when its spec changes. Best-effort — never blocks project convergence.
async fn converge_adminer(docker: &Docker, wireguard_ip: &str) -> Result<()> {
    ensure_network(docker, ADMIN_NETWORK).await?;
    // Put postgres (the engine humans browse) on the admin network so Adminer
    // can reach it by name. Best-effort: postgres may not exist yet, or may
    // already be attached — both are fine.
    let _ = docker
        .connect_network(
            ADMIN_NETWORK,
            bollard::models::NetworkConnectRequest {
                container: crate::db::engine_container(DbEngine::Postgres).to_string(),
                ..Default::default()
            },
        )
        .await;

    let env = vec!["ADMINER_DEFAULT_SERVER=majnet-postgres".to_string()];
    let hash = adminer_hash(&env, wireguard_ip);
    if running_with_hash(docker, ADMINER_NAME, &hash).await? {
        return Ok(());
    }
    ensure_image(docker, ADMINER_IMAGE).await?;
    remove_container(docker, ADMINER_NAME).await;
    // Publish the browser on the prod node's WireGuard IP only — the tailnet
    // Caddy dials `<wireguard_ip>:8081`; never bound to a public interface.
    let port_bindings = HashMap::from([(
        format!("{ADMINER_CONTAINER_PORT}/tcp"),
        Some(vec![PortBinding {
            host_ip: Some(wireguard_ip.to_string()),
            host_port: Some(ADMINER_HOST_PORT.into()),
        }]),
    )]);
    let created = docker
        .create_container(
            Some(qp::CreateContainerOptions {
                name: Some(ADMINER_NAME.into()),
                ..Default::default()
            }),
            ContainerCreateBody {
                image: Some(ADMINER_IMAGE.into()),
                env: Some(env),
                labels: Some(HashMap::from([(LABEL_CONFIG.to_string(), hash)])),
                exposed_ports: Some(vec![format!("{ADMINER_CONTAINER_PORT}/tcp")]),
                host_config: Some(HostConfig {
                    network_mode: Some(ADMIN_NETWORK.into()),
                    port_bindings: Some(port_bindings),
                    memory: Some(ADMINER_MEM),
                    nano_cpus: Some(ADMINER_NANO_CPUS),
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
        .context("creating adminer")?;
    docker
        .start_container(&created.id, None::<qp::StartContainerOptions>)
        .await
        .context("starting adminer")?;
    tracing::info!("adminer deployed");
    Ok(())
}

fn adminer_hash(env: &[String], wireguard_ip: &str) -> String {
    let mut h = Sha256::new();
    h.update(ADMINER_IMAGE.as_bytes());
    h.update(ADMIN_NETWORK.as_bytes());
    for e in env {
        h.update(e.as_bytes());
        h.update([0]);
    }
    h.update(ADMINER_MEM.to_le_bytes());
    h.update(ADMINER_NANO_CPUS.to_le_bytes());
    h.update(wireguard_ip.as_bytes());
    h.update(ADMINER_HOST_PORT.as_bytes());
    h.update(ADMINER_CONTAINER_PORT.as_bytes());
    hex::encode(h.finalize())[..16].to_string()
}

async fn converge_edge_main(
    docker: &Docker,
    platform: &Snapshot,
    age_key_dir: &Path,
) -> Result<()> {
    // Traefik's config from the platform repo (platform/edge-main/traefik/*).
    let prefix = "platform/edge-main/traefik/";
    let mut config: BTreeMap<String, Vec<u8>> = platform
        .files
        .iter()
        .filter_map(|(p, c)| {
            p.strip_prefix(prefix)
                .map(|rel| (rel.to_string(), c.clone()))
        })
        .collect();
    ensure!(
        config.contains_key("traefik.yaml"),
        "platform/edge-main/traefik/traefik.yaml missing from the platform repo"
    );

    // Origin certs the bot issued + committed (ADR 0007): `<zone>.crt` plus the
    // age-encrypted `<zone>.key.age`. Decrypt each key and stage cert+key for
    // the /certs mount; generate a dynamic TLS file so Traefik serves the right
    // cert per SNI.
    let cert_prefix = "platform/edge-main/certs/";
    let zones: Vec<String> = platform
        .files
        .keys()
        .filter_map(|p| p.strip_prefix(cert_prefix)?.strip_suffix(".crt"))
        .map(String::from)
        .collect();
    let mut cert_files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for zone in &zones {
        cert_files.insert(
            format!("{zone}.crt"),
            platform.files[&format!("{cert_prefix}{zone}.crt")].clone(),
        );
        if let Some(enc) = platform.files.get(&format!("{cert_prefix}{zone}.key.age")) {
            let key = age_decrypt(age_key_dir, enc)
                .await
                .with_context(|| format!("decrypting origin key for {zone}"))?;
            cert_files.insert(format!("{zone}.key"), key.into_bytes());
        }
    }
    if !zones.is_empty() {
        let mut tls = String::from("tls:\n  certificates:\n");
        for zone in &zones {
            tls.push_str(&format!(
                "    - certFile: /certs/{zone}.crt\n      keyFile: /certs/{zone}.key\n"
            ));
        }
        config.insert("dynamic/majnet-certs.yaml".into(), tls.into_bytes());
    }

    // Image + all config + certs → hash. Any change forces a recreate; an
    // unchanged, running edge-main is left alone.
    let hash = config_hash(&config, &cert_files);
    if running_with_hash(docker, "edge-main", &hash).await? {
        return Ok(());
    }

    ensure_network(docker, EDGE_NETWORK).await?;
    ensure_image(docker, EDGE_IMAGE).await?;
    ensure_image(docker, HELPER_IMAGE).await?;
    deliver_files(docker, EDGE_CONFIG_DIR, &config).await?;
    if !cert_files.is_empty() {
        deliver_files(docker, ORIGIN_CERTS_DIR, &cert_files).await?;
    }
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
                    memory: Some(EDGE_MEM),
                    nano_cpus: Some(EDGE_NANO_CPUS),
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

// ── managed DB engines (§15) ─────────────────────────────────────────────────

const DB_ROOT_DIR: &str = "/etc/majnet/db-root";

/// Container spec for a managed DB engine — mirrors
/// `platform/databases/compose.yaml`, but the reconciler owns the deploy over
/// the Docker API (same path as edge-main). The root password lives in a
/// secret file (delivered separately), never as a plain env var.
struct EngineSpec {
    image: &'static str,
    /// `KEY=VALUE` container env.
    env: Vec<String>,
    /// Optional entrypoint override (valkey seeds its ACL file on first boot).
    cmd: Option<Vec<String>>,
    /// Host binds: the named data volume + the read-only root-secret file.
    binds: Vec<String>,
    /// Basename of the root-secret file under DB_ROOT_DIR.
    secret: &'static str,
    /// Readiness probe run inside the container (exit 0 = accepting
    /// authenticated connections).
    ready: String,
    /// Memory cap (bytes) and CPU cap (nano-CPUs) for the engine container.
    mem: i64,
    nano_cpus: i64,
}

fn engine_spec(engine: DbEngine) -> EngineSpec {
    let secret = match engine {
        DbEngine::Postgres => "postgres",
        DbEngine::Mariadb => "mariadb",
        DbEngine::Valkey => "valkey",
        DbEngine::Mongodb => "mongodb",
    };
    let root_file = format!("/run/secrets/{secret}-root");
    let root_bind = format!("{DB_ROOT_DIR}/{secret}:{root_file}:ro");
    match engine {
        DbEngine::Postgres => EngineSpec {
            image: "postgres:17",
            env: vec![
                // Superuser stays inside the container (local socket trust);
                // per-app users authenticate over TCP with scram.
                "POSTGRES_HOST_AUTH_METHOD=scram-sha-256".into(),
                format!("POSTGRES_PASSWORD_FILE={root_file}"),
            ],
            cmd: None,
            binds: vec!["postgres-data:/var/lib/postgresql/data".into(), root_bind],
            secret,
            ready: "pg_isready -U postgres -q".into(),
            mem: 1024 * MB,
            nano_cpus: 1_000_000_000, // 1.0 CPU
        },
        DbEngine::Mariadb => EngineSpec {
            image: "mariadb:11",
            env: vec![format!("MARIADB_ROOT_PASSWORD_FILE={root_file}")],
            cmd: None,
            binds: vec!["mariadb-data:/var/lib/mysql".into(), root_bind],
            secret,
            ready: format!(r#"mariadb -uroot -p"$(cat {root_file})" -e "SELECT 1" >/dev/null 2>&1"#),
            mem: 1024 * MB,
            nano_cpus: 1_000_000_000,
        },
        DbEngine::Valkey => EngineSpec {
            image: "valkey/valkey:8",
            env: vec![],
            // The default user's password must live in the acl file
            // (requirepass is ignored once --aclfile is set); seed it once.
            cmd: Some(vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "test -s /data/users.acl || printf 'user default on >%s ~* &* +@all\\n' \"$(cat {root_file})\" > /data/users.acl; exec docker-entrypoint.sh valkey-server --aclfile /data/users.acl"
                ),
            ]),
            binds: vec!["valkey-data:/data".into(), root_bind],
            secret,
            ready: format!(
                r#"valkey-cli -a "$(cat {root_file})" --no-auth-warning ping >/dev/null 2>&1"#
            ),
            mem: 512 * MB,
            nano_cpus: 500_000_000,
        },
        DbEngine::Mongodb => EngineSpec {
            image: "mongo:8",
            env: vec![
                "MONGO_INITDB_ROOT_USERNAME=root".into(),
                format!("MONGO_INITDB_ROOT_PASSWORD_FILE={root_file}"),
            ],
            cmd: None,
            binds: vec!["mongodb-data:/data/db".into(), root_bind],
            secret,
            ready: format!(
                r#"mongosh --quiet -u root -p "$(cat {root_file})" --authenticationDatabase admin --eval 'db.runCommand({{ ping: 1 }})' >/dev/null 2>&1"#
            ),
            mem: 1024 * MB,
            nano_cpus: 1_000_000_000,
        },
    }
}

/// Ensure the managed engine for `engine` is running on this node (deploying it
/// on first use), then block until it accepts connections. On-demand so a node
/// only runs the engines its apps actually declare; idempotent via a config-hash
/// label like edge-main. The reconciler holds the DB master key and derives the
/// engine's root password statelessly, seeding it into the root-secret file.
pub async fn ensure_engine(config: &Config, docker: &Docker, engine: DbEngine) -> Result<()> {
    let name = crate::db::engine_container(engine);
    let root_pw = crate::db::root_password(config, engine)?;
    let spec = engine_spec(engine);

    let hash = engine_hash(&spec, &root_pw);
    if running_with_hash(docker, name, &hash).await? {
        return Ok(());
    }

    // The root-secret file must exist before the (single-file) bind mount, or
    // Docker would create a directory in its place.
    ensure_image(docker, HELPER_IMAGE).await?;
    deliver_files(
        docker,
        DB_ROOT_DIR,
        &BTreeMap::from([(spec.secret.to_string(), root_pw.into_bytes())]),
    )
    .await
    .context("delivering engine root secret")?;
    ensure_image(docker, spec.image).await?;
    remove_container(docker, name).await;

    let created = docker
        .create_container(
            Some(qp::CreateContainerOptions {
                name: Some(name.into()),
                ..Default::default()
            }),
            ContainerCreateBody {
                image: Some(spec.image.into()),
                cmd: spec.cmd.clone(),
                env: Some(spec.env.clone()),
                labels: Some(HashMap::from([(LABEL_CONFIG.to_string(), hash)])),
                host_config: Some(HostConfig {
                    binds: Some(spec.binds.clone()),
                    memory: Some(spec.mem),
                    nano_cpus: Some(spec.nano_cpus),
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
        .with_context(|| format!("creating {name}"))?;
    docker
        .start_container(&created.id, None::<qp::StartContainerOptions>)
        .await
        .with_context(|| format!("starting {name}"))?;
    tracing::info!(?engine, "DB engine deployed; waiting for readiness");
    wait_ready(docker, name, &spec.ready)
        .await
        .with_context(|| format!("{name} did not become ready"))?;
    tracing::info!(?engine, "DB engine ready");
    Ok(())
}

fn engine_hash(spec: &EngineSpec, root_pw: &str) -> String {
    let mut h = Sha256::new();
    h.update(spec.image.as_bytes());
    for e in &spec.env {
        h.update(e.as_bytes());
        h.update([0]);
    }
    for c in spec.cmd.iter().flatten() {
        h.update(c.as_bytes());
        h.update([0]);
    }
    for b in &spec.binds {
        h.update(b.as_bytes());
        h.update([0]);
    }
    h.update(spec.mem.to_le_bytes());
    h.update(spec.nano_cpus.to_le_bytes());
    h.update(root_pw.as_bytes());
    hex::encode(h.finalize())[..16].to_string()
}

/// Poll the readiness probe until it exits 0 (engines take seconds to initdb).
/// ~60 s ceiling, then give up so the loop records a failure and retries.
async fn wait_ready(docker: &Docker, name: &str, probe: &str) -> Result<()> {
    for _ in 0..31 {
        if exec_ok(docker, name, probe).await {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    anyhow::bail!("still not ready after ~60s")
}

/// Run `script` in `container`; true iff it exited 0. Tolerates a not-yet-running
/// container (returns false) so readiness polling can start immediately.
async fn exec_ok(docker: &Docker, container: &str, script: &str) -> bool {
    use futures_util::StreamExt;
    let make = docker
        .create_exec(
            container,
            ExecConfig {
                cmd: Some(vec!["sh".into(), "-c".into(), script.into()]),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await;
    let Ok(exec) = make else { return false };
    match docker
        .start_exec(&exec.id, None::<bollard::exec::StartExecOptions>)
        .await
    {
        Ok(bollard::exec::StartExecResults::Attached {
            output: mut stream, ..
        }) => while stream.next().await.is_some() {},
        Ok(_) => {}
        Err(_) => return false,
    }
    docker
        .inspect_exec(&exec.id)
        .await
        .ok()
        .and_then(|i| i.exit_code)
        == Some(0)
}

fn config_hash(config: &BTreeMap<String, Vec<u8>>, certs: &BTreeMap<String, Vec<u8>>) -> String {
    let mut h = Sha256::new();
    h.update(EDGE_IMAGE.as_bytes());
    h.update(EDGE_MEM.to_le_bytes());
    h.update(EDGE_NANO_CPUS.to_le_bytes());
    for map in [config, certs] {
        for (path, content) in map {
            h.update(path.as_bytes());
            h.update([0]);
            h.update(content);
            h.update([0]);
        }
    }
    hex::encode(h.finalize())[..16].to_string()
}

/// Decrypt an age ciphertext with the `age-production` key via the `age` binary.
pub(crate) async fn age_decrypt(age_key_dir: &Path, ciphertext: &[u8]) -> Result<String> {
    use tokio::io::AsyncWriteExt;
    let key = age_key_dir.join("age-production.key");
    let mut child = tokio::process::Command::new("age")
        .args(["-d", "-i", key.to_str().context("non-utf8 age key path")?])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning age (is it installed?)")?;
    child
        .stdin
        .take()
        .context("no stdin")?
        .write_all(ciphertext)
        .await?;
    let out = child.wait_with_output().await?;
    anyhow::ensure!(
        out.status.success(),
        "age decrypt failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8(out.stdout)?)
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
pub(crate) async fn deliver_files(
    docker: &Docker,
    dir: &str,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
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
