//! Native alerting: a periodic evaluator that checks node/host metrics + site
//! health and posts state transitions to a configurable Discord webhook. No
//! extra services — reuses the reconciler's metrics gathering + reqwest. Config
//! (webhook, thresholds) + the currently-firing set live in the reconciler
//! store, set from the dashboard Settings page.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use crate::AppState;

const INTERVAL: Duration = Duration::from_secs(60);

/// The control-plane bot health endpoint (WG-internal) — always watched. The
/// public app sites are discovered from the deployed manifests, not hardcoded,
/// so a domain change or rename keeps monitoring in sync automatically.
const CONTROL_PLANE: (&str, &str) = ("control-plane bot", "http://10.88.0.1:8081/healthz");

pub async fn run_loop(state: Arc<AppState>) {
    loop {
        tokio::time::sleep(INTERVAL).await;
        if let Err(e) = tick(&state).await {
            tracing::warn!(error = format!("{e:#}"), "alert evaluator tick failed");
        }
    }
}

async fn tick(state: &AppState) -> anyhow::Result<()> {
    let webhook = state.store.get_config("alert_webhook")?.unwrap_or_default();
    if webhook.trim().is_empty() {
        return Ok(()); // alerting disabled until a webhook is configured
    }
    let cpu_thr = threshold(state, "alert_cpu_pct");
    let mem_thr = threshold(state, "alert_mem_pct");

    // key -> human label for everything currently in an alerting state.
    let mut current: BTreeMap<String, String> = BTreeMap::new();

    let enrolled = enrolled_nodes(state).await.unwrap_or_default();
    for n in crate::metrics::gather(state).await.unwrap_or_default() {
        if !n.reachable {
            // Only alert on nodes that are supposed to be up (skip the parked one).
            if enrolled.contains(&n.name) {
                current.insert(
                    format!("node:{}:down", n.name),
                    format!("Node **{}** unreachable", n.name),
                );
            }
            continue;
        }
        if n.host_cpu_pct > cpu_thr {
            current.insert(
                format!("node:{}:cpu", n.name),
                format!(
                    "Node **{}** CPU {:.0}% (> {:.0}%)",
                    n.name, n.host_cpu_pct, cpu_thr
                ),
            );
        }
        let mem_pct = if n.mem_total > 0 {
            n.mem_used as f64 / n.mem_total as f64 * 100.0
        } else {
            0.0
        };
        if mem_pct > mem_thr {
            current.insert(
                format!("node:{}:mem", n.name),
                format!(
                    "Node **{}** memory {:.0}% (> {:.0}%)",
                    n.name, mem_pct, mem_thr
                ),
            );
        }
    }

    for (name, url) in discover_sites(state).await {
        if !site_ok(state, &url).await {
            current.insert(
                format!("site:{name}"),
                format!("**{name}** is DOWN — {url}"),
            );
        }
    }

    // Debounce: only alert on a condition that persisted across two consecutive
    // ticks (~2 min). A single-tick blip — the brief unroutable window while an
    // app is archived/deleted/renamed or a deploy cuts over — never fires.
    let last: BTreeMap<String, String> = load_map(state, "alert_seen");
    let firing: BTreeMap<String, String> = current
        .iter()
        .filter(|(k, _)| last.contains_key(*k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let prev: BTreeMap<String, String> = load_map(state, "alert_firing");
    for (k, label) in &firing {
        if !prev.contains_key(k) {
            post(state, &webhook, &format!("🔴 {label}")).await;
        }
    }
    for (k, label) in &prev {
        if !firing.contains_key(k) {
            post(state, &webhook, &format!("🟢 Recovered — {label}")).await;
        }
    }
    state
        .store
        .set_config("alert_seen", &serde_json::to_string(&current)?)?;
    state
        .store
        .set_config("alert_firing", &serde_json::to_string(&firing)?)?;
    Ok(())
}

/// Load a JSON string→string map from config, defaulting to empty.
fn load_map(state: &AppState, key: &str) -> BTreeMap<String, String> {
    state
        .store
        .get_config(key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn threshold(state: &AppState, key: &str) -> f64 {
    state
        .store
        .get_config(key)
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(90.0)
}

async fn enrolled_nodes(state: &AppState) -> anyhow::Result<HashSet<String>> {
    let platform = crate::snapshot::fetch(
        &state.http,
        &state.config,
        &state.config.root_org,
        "platform",
        "main",
    )
    .await?
    .context("platform snapshot unavailable")?;
    let nodes = majnet_common::platform::NodesFile::parse(
        platform.files.get("nodes.yaml").context("no nodes.yaml")?,
    )?;
    Ok(nodes
        .nodes
        .iter()
        .filter(|n| !n.wireguard_pubkey.is_empty())
        .map(|n| n.name.clone())
        .collect())
}

/// Sites to probe: the control-plane bot plus every production ingress host of
/// every deployed app, read from the `env/production` branches. Derived rather
/// than hardcoded, so a domain change or app/project rename keeps the monitored
/// set in sync on the next tick. Degrades to just the control-plane check if the
/// registry/branches can't be read.
async fn discover_sites(state: &AppState) -> Vec<(String, String)> {
    let mut sites = vec![(CONTROL_PLANE.0.to_string(), CONTROL_PLANE.1.to_string())];
    if let Ok(Some(platform)) = crate::snapshot::fetch(
        &state.http,
        &state.config,
        &state.config.root_org,
        "platform",
        "main",
    )
    .await
    {
        if let Some(projects) = platform
            .files
            .get("projects.yaml")
            .and_then(|b| majnet_common::platform::ProjectsFile::parse(b).ok())
        {
            for proj in projects.projects {
                let Ok(Some(env)) = crate::snapshot::fetch(
                    &state.http,
                    &state.config,
                    &proj.org,
                    "ops",
                    "env/production",
                )
                .await
                else {
                    continue;
                };
                for (path, bytes) in &env.files {
                    // `<app>.yaml` at the root — skip `secrets/<app>.yaml`.
                    if !path.ends_with(".yaml") || path.contains('/') {
                        continue;
                    }
                    let Ok(manifest) = std::str::from_utf8(bytes)
                        .ok()
                        .context("utf8")
                        .and_then(majnet_common::manifest::AppManifest::parse)
                    else {
                        continue;
                    };
                    if let Some(ingress) = manifest.ingress {
                        for h in ingress.hosts() {
                            sites.push((h.to_string(), format!("https://{h}/")));
                        }
                    }
                }
            }
        }
    }
    sites.sort();
    sites.dedup();
    sites
}

async fn site_ok(state: &AppState, url: &str) -> bool {
    // Two attempts to avoid flapping on a single blip.
    for _ in 0..2 {
        if let Ok(r) = state
            .http
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            if r.status().is_success() {
                return true;
            }
        }
    }
    false
}

async fn post(state: &AppState, webhook: &str, content: &str) {
    let _ = state
        .http
        .post(webhook)
        .json(&serde_json::json!({ "content": content }))
        .timeout(Duration::from_secs(10))
        .send()
        .await;
}

/// Send a one-off test message to the configured webhook.
pub async fn send_test(state: &AppState) -> anyhow::Result<()> {
    let webhook = state.store.get_config("alert_webhook")?.unwrap_or_default();
    anyhow::ensure!(!webhook.trim().is_empty(), "no webhook configured");
    state
        .http
        .post(&webhook)
        .json(&serde_json::json!({ "content": "✅ MajNet alerts — test message; the webhook works." }))
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}
