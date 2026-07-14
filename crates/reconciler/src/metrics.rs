//! On-demand node + container metrics, gathered over the same per-node Docker
//! API (mTLS over WireGuard) the reconciler already uses to deploy — no
//! monitoring agents, no extra services. Surfaced read-only to the dashboard.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use majnet_common::platform::{Node, NodesFile};
use serde::Serialize;
use std::time::Duration;

use crate::AppState;

#[derive(Serialize)]
pub struct NodeMetrics {
    pub name: String,
    pub role: String,
    pub reachable: bool,
    pub error: Option<String>,
    pub cpus: i64,
    pub host_cpu_pct: f64,
    pub mem_total: i64,
    pub mem_used: i64,
    pub disk_images: i64,
    pub containers: i64,
    pub containers_running: i64,
    pub server_version: String,
    pub os: String,
    pub kernel: String,
    pub apps: Vec<ContainerMetric>,
}

#[derive(Serialize)]
pub struct ContainerMetric {
    pub name: String,
    pub image: String,
    pub state: String,
    pub cpu_pct: f64,
    pub mem_used: u64,
    pub mem_limit: u64,
}

/// Metrics for every node in `nodes.yaml`. Each node is probed with a short
/// timeout so an unreachable node (e.g. the parked private one) is reported as
/// such rather than hanging the whole response.
pub async fn gather(state: &AppState) -> Result<Vec<NodeMetrics>> {
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

    let mut out = Vec::new();
    for node in &nodes.nodes {
        let mut m = NodeMetrics {
            name: node.name.clone(),
            role: node.role.clone(),
            reachable: false,
            error: None,
            cpus: 0,
            host_cpu_pct: 0.0,
            mem_total: 0,
            mem_used: 0,
            disk_images: 0,
            containers: 0,
            containers_running: 0,
            server_version: String::new(),
            os: String::new(),
            kernel: String::new(),
            apps: Vec::new(),
        };
        match tokio::time::timeout(
            Duration::from_secs(20),
            collect(state, &nodes, node, &mut m),
        )
        .await
        {
            Ok(Ok(())) => m.reachable = true,
            Ok(Err(e)) => m.error = Some(format!("{e:#}")),
            Err(_) => m.error = Some("timeout — node unreachable".into()),
        }
        out.push(m);
    }
    Ok(out)
}

async fn collect(
    state: &AppState,
    nodes: &NodesFile,
    node: &Node,
    m: &mut NodeMetrics,
) -> Result<()> {
    let docker = state.nodes(nodes).client_for(node).await?;

    // Read info/df via their JSON (stable Docker field names) to avoid brittle
    // bollard struct-field coupling.
    let info = serde_json::to_value(&docker.info().await?).unwrap_or_default();
    m.cpus = info["NCPU"].as_i64().unwrap_or(0);
    m.mem_total = info["MemTotal"].as_i64().unwrap_or(0);
    m.containers = info["Containers"].as_i64().unwrap_or(0);
    m.containers_running = info["ContainersRunning"].as_i64().unwrap_or(0);
    m.server_version = info["ServerVersion"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    m.os = info["OperatingSystem"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    m.kernel = info["KernelVersion"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    if let Ok(df) = docker
        .df(None::<bollard::query_parameters::DataUsageOptions>)
        .await
    {
        let dv = serde_json::to_value(&df).unwrap_or_default();
        m.disk_images = dv["LayersSize"].as_i64().unwrap_or(0);
    }

    let list = docker
        .list_containers(Some(bollard::query_parameters::ListContainersOptions {
            all: false,
            ..Default::default()
        }))
        .await?;
    // Per-container stats concurrently — a one-shot `stats` read blocks ~1s each,
    // so serially over many containers would blow the node budget. Run the host
    // /proc probe alongside them.
    let apps_fut = futures_util::future::join_all(list.into_iter().map(|c| {
        let docker = docker.clone();
        async move {
            let mut cm = ContainerMetric {
                name: c
                    .names
                    .as_ref()
                    .and_then(|n| n.first())
                    .map(|s| s.trim_start_matches('/').to_string())
                    .unwrap_or_default(),
                image: c.image.clone().unwrap_or_default(),
                state: c
                    .state
                    .map(|s| format!("{s:?}").to_lowercase())
                    .unwrap_or_default(),
                cpu_pct: 0.0,
                mem_used: 0,
                mem_limit: 0,
            };
            if let Some(id) = &c.id {
                let mut s = docker.stats(
                    id,
                    Some(bollard::query_parameters::StatsOptions {
                        stream: false,
                        one_shot: false,
                    }),
                );
                if let Ok(Some(Ok(stat))) =
                    tokio::time::timeout(Duration::from_secs(4), s.next()).await
                {
                    if let Ok(v) = serde_json::to_value(&stat) {
                        cm.cpu_pct = cpu_percent(&v);
                        cm.mem_used = v["memory_stats"]["usage"].as_u64().unwrap_or(0);
                        cm.mem_limit = v["memory_stats"]["limit"].as_u64().unwrap_or(0);
                    }
                }
            }
            cm
        }
    }));
    let (apps, host) = tokio::join!(apps_fut, host_probe(&docker));
    m.apps = apps;
    if let Some((cpu, total, used)) = host {
        m.host_cpu_pct = cpu;
        m.mem_used = used;
        if m.mem_total == 0 {
            m.mem_total = total;
        }
    }
    Ok(())
}

/// Host CPU% + memory, read from `/proc` inside a throwaway busybox container.
/// On plain Docker (no lxcfs) `/proc/stat` and `/proc/meminfo` reflect the host,
/// so this needs no host shell, agent, or privileges — just the Docker API.
/// Returns (cpu_pct, mem_total_bytes, mem_used_bytes).
async fn host_probe(docker: &bollard::Docker) -> Option<(f64, i64, i64)> {
    use bollard::query_parameters as qp;
    if docker
        .inspect_image(crate::secrets::HELPER_IMAGE)
        .await
        .is_err()
    {
        let _ = docker
            .create_image(
                Some(qp::CreateImageOptions {
                    from_image: Some(crate::secrets::HELPER_IMAGE.into()),
                    ..Default::default()
                }),
                None,
                None,
            )
            .collect::<Vec<_>>()
            .await;
    }
    let script = "grep -E '^MemTotal|^MemAvailable' /proc/meminfo; echo ---; \
                  grep '^cpu ' /proc/stat; sleep 1; grep '^cpu ' /proc/stat";
    let helper = docker
        .create_container(
            None::<qp::CreateContainerOptions>,
            bollard::models::ContainerCreateBody {
                image: Some(crate::secrets::HELPER_IMAGE.into()),
                cmd: Some(vec!["sh".into(), "-c".into(), script.into()]),
                labels: Some([("majnet.helper".to_string(), "metrics".to_string())].into()),
                ..Default::default()
            },
        )
        .await
        .ok()?;

    let out = async {
        docker
            .start_container(&helper.id, None::<qp::StartContainerOptions>)
            .await?;
        // follow:true streams until the (short-lived) container exits.
        let mut logs = docker.logs(
            &helper.id,
            Some(qp::LogsOptions {
                stdout: true,
                stderr: false,
                follow: true,
                ..Default::default()
            }),
        );
        let mut buf = String::new();
        while let Some(Ok(chunk)) = logs.next().await {
            buf.push_str(&chunk.to_string());
        }
        Ok::<_, anyhow::Error>(buf)
    }
    .await;

    let _ = docker
        .remove_container(
            &helper.id,
            Some(qp::RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;

    parse_proc(&out.ok()?)
}

fn parse_proc(s: &str) -> Option<(f64, i64, i64)> {
    let (mem, cpu) = s.split_once("---")?;
    let kb = |key: &str| -> Option<i64> {
        mem.lines()
            .find(|l| l.starts_with(key))?
            .split_whitespace()
            .nth(1)?
            .parse::<i64>()
            .ok()
            .map(|v| v * 1024)
    };
    let mem_total = kb("MemTotal")?;
    let mem_used = mem_total - kb("MemAvailable").unwrap_or(0);

    // Two `cpu ...` samples → busy fraction over the interval.
    let sample = |line: &str| -> Option<(f64, f64)> {
        let n: Vec<f64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|x| x.parse::<f64>().ok())
            .collect();
        if n.len() < 5 {
            return None;
        }
        let idle = n[3] + n[4]; // idle + iowait
        Some((n.iter().sum(), idle))
    };
    let mut cpu_lines = cpu.lines().filter(|l| l.starts_with("cpu "));
    let (t1, i1) = sample(cpu_lines.next()?)?;
    let (t2, i2) = sample(cpu_lines.next()?)?;
    let td = t2 - t1;
    let cpu_pct = if td > 0.0 {
        (((1.0 - (i2 - i1) / td) * 100.0 * 100.0).round() / 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    Some((cpu_pct, mem_total, mem_used))
}

/// Docker's container CPU% — the same formula `docker stats` uses, read from the
/// stats JSON (stable field names) to avoid brittle nested struct access.
fn cpu_percent(v: &serde_json::Value) -> f64 {
    let cur = v["cpu_stats"]["cpu_usage"]["total_usage"]
        .as_f64()
        .unwrap_or(0.0);
    let pre = v["precpu_stats"]["cpu_usage"]["total_usage"]
        .as_f64()
        .unwrap_or(0.0);
    let sys_cur = v["cpu_stats"]["system_cpu_usage"].as_f64().unwrap_or(0.0);
    let sys_pre = v["precpu_stats"]["system_cpu_usage"]
        .as_f64()
        .unwrap_or(0.0);
    let online = v["cpu_stats"]["online_cpus"]
        .as_f64()
        .unwrap_or(1.0)
        .max(1.0);
    let cpu_delta = cur - pre;
    let sys_delta = sys_cur - sys_pre;
    if sys_delta > 0.0 && cpu_delta > 0.0 {
        ((cpu_delta / sys_delta) * online * 100.0 * 100.0).round() / 100.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::parse_proc;

    #[test]
    fn parses_meminfo_and_cpu_delta() {
        // idle goes 100→160 (Δ60) out of total 200→300 (Δ100) → 40% busy.
        let s = "MemTotal:       1000 kB\nMemAvailable:    400 kB\n---\n\
                 cpu  50 0 50 100 0 0 0 0\ncpu  90 0 50 160 0 0 0 0\n";
        let (cpu, total, used) = parse_proc(s).unwrap();
        assert_eq!(total, 1000 * 1024);
        assert_eq!(used, 600 * 1024);
        assert!((cpu - 40.0).abs() < 0.01, "cpu={cpu}");
    }
}
