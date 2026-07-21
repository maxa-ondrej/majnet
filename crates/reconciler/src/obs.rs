//! Observability query layer (ADR 0023 phase 3) — backs the dashboard's
//! per-app Observability tab.
//!
//! The reconciler is the runtime-state plane (metrics, logs, containers) and is
//! on the WireGuard mesh, so it queries the Tempo (traces) and Loki (logs) query
//! APIs that the observability backend publishes on the private node's WG IP
//! (`wg_ports`, ADR 0023). Endpoints are configured via `MAJNET_TEMPO_ENDPOINT` /
//! `MAJNET_LOKI_ENDPOINT`; unset ⇒ 503 so the tab degrades gracefully.
//!
//! Apps are identified in the telemetry by `service.name` = the app's manifest
//! name (the reconciler injects `OTEL_RESOURCE_ATTRIBUTES` at deploy). RED tiles
//! are computed here from a Tempo search over the window — Tempo's TraceQL
//! metrics API needs the metrics-generator, which the deployed stack doesn't run.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppState;

/// How many traces / log lines a single query pulls back. RED over the window is
/// approximate when a busy app exceeds this (flagged via `capped`).
const QUERY_LIMIT: usize = 200;
const DEFAULT_WINDOW_MIN: u64 = 15;

#[derive(Deserialize)]
pub struct WindowQuery {
    /// Look-back window in minutes (default 15).
    window_min: Option<u64>,
}

/// A single trace in the recent-traces list.
#[derive(Serialize)]
pub struct TraceRow {
    trace_id: String,
    root_service: String,
    root_name: String,
    duration_ms: u64,
    start_unix_nano: u64,
    error: bool,
}

/// RED (rate / errors / duration) golden signals for the window, computed from
/// the sampled trace set.
#[derive(Serialize)]
pub struct Red {
    rate_per_min: f64,
    error_pct: f64,
    p95_ms: u64,
    window_min: u64,
    sampled: usize,
    /// The sample hit `QUERY_LIMIT` — rate/error are lower bounds for this window.
    capped: bool,
}

#[derive(Serialize)]
pub struct Overview {
    red: Red,
    traces: Vec<TraceRow>,
}

/// One span in a trace waterfall (offset + depth precomputed for rendering).
#[derive(Serialize)]
pub struct SpanRow {
    span_id: String,
    parent_id: String,
    service: String,
    name: String,
    start_offset_ms: f64,
    duration_ms: f64,
    depth: u32,
    error: bool,
}

#[derive(Serialize)]
pub struct TraceDetail {
    trace_id: String,
    duration_ms: f64,
    spans: Vec<SpanRow>,
}

#[derive(Serialize)]
pub struct LogRow {
    ts_unix_nano: u64,
    level: String,
    service: String,
    msg: String,
    trace_id: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read-only telemetry: gate like logs — Admin for production, Developer else.
async fn authz(
    state: &AppState,
    headers: &HeaderMap,
    project: &str,
    class: &str,
) -> Result<(), (StatusCode, String)> {
    let class_e: majnet_common::EnvClass = serde_yaml::from_str(class).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "class must be production|stable|testing|ephemeral".into(),
        )
    })?;
    let min_role = if class_e == majnet_common::EnvClass::Production {
        majnet_common::project::Role::Admin
    } else {
        majnet_common::project::Role::Developer
    };
    crate::authz::require(state, headers, project, min_role)
        .await
        .map(|_| ())
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))
}

// ---- Tempo -----------------------------------------------------------------

/// `GET /api/obs/{org}/{project}/{class}/{app}/overview` — RED tiles + recent
/// traces for the app over the window.
pub async fn overview(
    State(state): State<Arc<AppState>>,
    Path((project, class, app)): Path<(String, String, String)>,
    Query(q): Query<WindowQuery>,
    headers: HeaderMap,
) -> Result<Json<Overview>, (StatusCode, String)> {
    authz(&state, &headers, &project, &class).await?;
    let Some(tempo) = state.config.tempo_endpoint.clone() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Tempo endpoint not configured".into(),
        ));
    };
    let window_min = q.window_min.unwrap_or(DEFAULT_WINDOW_MIN).clamp(1, 1440);
    overview_inner(&state, &tempo, &app, &class, window_min)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn overview_inner(
    state: &AppState,
    tempo: &str,
    app: &str,
    class: &str,
    window_min: u64,
) -> Result<Overview> {
    let end = now_secs();
    let start = end.saturating_sub(window_min * 60);

    // Traces the app served in this environment, over the window. Scope by the
    // injected `deployment.environment` resource attr so the tab matches the
    // selected class.
    let sel =
        format!("resource.service.name=\"{app}\" && resource.deployment.environment=\"{class}\"");
    let all = tempo_search(state, tempo, &format!("{{{sel}}}"), start, end).await?;
    // Error traces (their IDs) so we can flag rows + compute the error rate.
    let errs = tempo_search(
        state,
        tempo,
        &format!("{{{sel} && status=error}}"),
        start,
        end,
    )
    .await
    .unwrap_or_default();
    let err_ids: std::collections::HashSet<&str> = errs
        .iter()
        .filter_map(|t| t.get("traceID").and_then(Value::as_str))
        .collect();

    let mut durations: Vec<u64> = Vec::with_capacity(all.len());
    let mut traces: Vec<TraceRow> = Vec::with_capacity(all.len());
    for t in &all {
        let trace_id = t
            .get("traceID")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if trace_id.is_empty() {
            continue;
        }
        let duration_ms = t.get("durationMs").and_then(Value::as_u64).unwrap_or(0);
        durations.push(duration_ms);
        let error = err_ids.contains(trace_id.as_str());
        traces.push(TraceRow {
            error,
            root_service: t
                .get("rootServiceName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            root_name: t
                .get("rootTraceName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            duration_ms,
            start_unix_nano: t
                .get("startTimeUnixNano")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            trace_id,
        });
    }
    // Most-recent first.
    traces.sort_by_key(|t| std::cmp::Reverse(t.start_unix_nano));

    let sampled = all.len();
    let capped = sampled >= QUERY_LIMIT;
    let error_pct = if sampled == 0 {
        0.0
    } else {
        err_ids.len() as f64 / sampled as f64 * 100.0
    };
    let red = Red {
        rate_per_min: sampled as f64 / window_min as f64,
        error_pct: (error_pct * 10.0).round() / 10.0,
        p95_ms: percentile(&mut durations, 95.0),
        window_min,
        sampled,
        capped,
    };
    Ok(Overview { red, traces })
}

/// `GET /api/obs/{org}/trace/{trace_id}` — the span waterfall for one trace.
pub async fn trace(
    State(state): State<Arc<AppState>>,
    Path(trace_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<TraceDetail>, (StatusCode, String)> {
    // Platform-admin only (a trace can span any project's services).
    crate::authz::require_platform_admin(&state, &headers)
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("{e:#}")))?;
    let Some(tempo) = state.config.tempo_endpoint.clone() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Tempo endpoint not configured".into(),
        ));
    };
    trace_inner(&state, &tempo, &trace_id)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn trace_inner(state: &AppState, tempo: &str, trace_id: &str) -> Result<TraceDetail> {
    let url = format!("{tempo}/api/traces/{trace_id}");
    let body: Value = state
        .http
        .get(&url)
        .send()
        .await
        .context("querying Tempo trace")?
        .error_for_status()
        .context("Tempo trace status")?
        .json()
        .await
        .context("parsing Tempo trace")?;

    // Flatten OTLP batches → (service, name, start_ns, end_ns, span_id, parent_id, error).
    struct Raw {
        span_id: String,
        parent_id: String,
        service: String,
        name: String,
        start_ns: u64,
        end_ns: u64,
        error: bool,
    }
    let mut raws: Vec<Raw> = Vec::new();
    for batch in body
        .get("batches")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let service =
            attr_str(batch.pointer("/resource/attributes"), "service.name").unwrap_or_default();
        for scope in batch
            .get("scopeSpans")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for span in scope
                .get("spans")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let start_ns = span.get("startTimeUnixNano").and_then(str_u64).unwrap_or(0);
                let end_ns = span
                    .get("endTimeUnixNano")
                    .and_then(str_u64)
                    .unwrap_or(start_ns);
                let status_code = span
                    .pointer("/status/code")
                    .and_then(|c| {
                        c.as_str()
                            .map(str::to_string)
                            .or_else(|| c.as_i64().map(|n| n.to_string()))
                    })
                    .unwrap_or_default();
                raws.push(Raw {
                    span_id: b64_to_hex(span.get("spanId").and_then(Value::as_str).unwrap_or("")),
                    parent_id: b64_to_hex(
                        span.get("parentSpanId")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    ),
                    service: service.clone(),
                    name: span
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    start_ns,
                    end_ns,
                    error: status_code == "STATUS_CODE_ERROR" || status_code == "2",
                });
            }
        }
    }
    let t0 = raws.iter().map(|r| r.start_ns).min().unwrap_or(0);
    let t_end = raws.iter().map(|r| r.end_ns).max().unwrap_or(t0);
    // Depth = length of the parent chain within this trace (owned keys so the
    // map outlives the borrow of `raws` below).
    let parent_of: std::collections::HashMap<String, String> = raws
        .iter()
        .map(|r| (r.span_id.clone(), r.parent_id.clone()))
        .collect();
    let depth_of = |start: &str| -> u32 {
        let mut id = start;
        let mut d = 0;
        while let Some(p) = parent_of.get(id) {
            if p.is_empty() || !parent_of.contains_key(p) || d > 64 {
                break;
            }
            d += 1;
            id = p;
        }
        d
    };
    let mut spans: Vec<SpanRow> = raws
        .iter()
        .map(|r| SpanRow {
            start_offset_ms: (r.start_ns.saturating_sub(t0)) as f64 / 1e6,
            duration_ms: (r.end_ns.saturating_sub(r.start_ns)) as f64 / 1e6,
            depth: depth_of(&r.span_id),
            error: r.error,
            span_id: r.span_id.clone(),
            parent_id: r.parent_id.clone(),
            service: r.service.clone(),
            name: r.name.clone(),
        })
        .collect();
    spans.sort_by(|a, b| {
        a.start_offset_ms
            .partial_cmp(&b.start_offset_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(TraceDetail {
        trace_id: trace_id.to_string(),
        duration_ms: (t_end.saturating_sub(t0)) as f64 / 1e6,
        spans,
    })
}

/// Run a Tempo TraceQL search over `[start, end]` (unix seconds); returns the
/// raw `traces` array.
async fn tempo_search(
    state: &AppState,
    tempo: &str,
    q: &str,
    start: u64,
    end: u64,
) -> Result<Vec<Value>> {
    let url = format!("{tempo}/api/search");
    let body: Value = state
        .http
        .get(&url)
        .query(&[
            ("q", q),
            ("limit", &QUERY_LIMIT.to_string()),
            ("start", &start.to_string()),
            ("end", &end.to_string()),
        ])
        .send()
        .await
        .context("querying Tempo search")?
        .error_for_status()
        .context("Tempo search status")?
        .json()
        .await
        .context("parsing Tempo search")?;
    Ok(body
        .get("traces")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

// ---- Loki ------------------------------------------------------------------

/// `GET /api/obs/{org}/{project}/{class}/{app}/logs` — structured OTEL logs for
/// the app from Loki, most-recent first, with trace_id for correlation.
pub async fn logs(
    State(state): State<Arc<AppState>>,
    Path((project, class, app)): Path<(String, String, String)>,
    Query(q): Query<WindowQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<LogRow>>, (StatusCode, String)> {
    authz(&state, &headers, &project, &class).await?;
    let Some(loki) = state.config.loki_endpoint.clone() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Loki endpoint not configured".into(),
        ));
    };
    let window_min = q.window_min.unwrap_or(DEFAULT_WINDOW_MIN).clamp(1, 1440);
    logs_inner(&state, &loki, &app, &class, window_min)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))
}

async fn logs_inner(
    state: &AppState,
    loki: &str,
    app: &str,
    class: &str,
    window_min: u64,
) -> Result<Vec<LogRow>> {
    let end_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let start_ns = end_ns.saturating_sub(window_min as u128 * 60 * 1_000_000_000);
    let url = format!("{loki}/loki/api/v1/query_range");
    let selector = format!("{{service_name=\"{app}\", deployment_environment=\"{class}\"}}");
    let body: Value = state
        .http
        .get(&url)
        .query(&[
            ("query", selector.as_str()),
            ("limit", &QUERY_LIMIT.to_string()),
            ("start", &start_ns.to_string()),
            ("end", &end_ns.to_string()),
            ("direction", "backward"),
        ])
        .send()
        .await
        .context("querying Loki")?
        .error_for_status()
        .context("Loki status")?
        .json()
        .await
        .context("parsing Loki response")?;

    let mut rows: Vec<LogRow> = Vec::new();
    for stream in body
        .pointer("/data/result")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let labels = stream.get("stream");
        let service = labels
            .and_then(|l| l.get("service_name"))
            .and_then(Value::as_str)
            .unwrap_or(app)
            .to_string();
        let level = labels
            .and_then(|l| l.get("severity_text").or_else(|| l.get("detected_level")))
            .and_then(Value::as_str)
            .unwrap_or("info")
            .to_lowercase();
        let trace_id = labels
            .and_then(|l| l.get("trace_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        for pair in stream
            .get("values")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let arr = pair.as_array();
            let ts = arr
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let msg = arr
                .and_then(|a| a.get(1))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            rows.push(LogRow {
                ts_unix_nano: ts,
                level: level.clone(),
                service: service.clone(),
                msg,
                trace_id: trace_id.clone(),
            });
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.ts_unix_nano));
    rows.truncate(QUERY_LIMIT);
    Ok(rows)
}

// ---- helpers ---------------------------------------------------------------

/// Find a string OTLP attribute by key within an `attributes` array node.
fn attr_str(attrs: Option<&Value>, key: &str) -> Option<String> {
    attrs?.as_array()?.iter().find_map(|a| {
        (a.get("key").and_then(Value::as_str) == Some(key))
            .then(|| {
                a.pointer("/value/stringValue")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .flatten()
    })
}

fn str_u64(v: &Value) -> Option<u64> {
    v.as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v.as_u64())
}

/// Tempo trace-by-id returns span/trace IDs base64-encoded; render them as hex
/// (the spelling every other tool uses).
fn b64_to_hex(b64: &str) -> String {
    use base64::Engine;
    match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(bytes) => hex::encode(bytes),
        Err(_) => b64.to_string(),
    }
}

/// The p-th percentile (0..100) of the values, in ms. Sorts in place.
fn percentile(vals: &mut [u64], p: f64) -> u64 {
    if vals.is_empty() {
        return 0;
    }
    vals.sort_unstable();
    let rank = (p / 100.0 * vals.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(vals.len() - 1);
    vals[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_picks_the_right_rank() {
        let mut v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&mut v, 95.0), 95);
        assert_eq!(percentile(&mut v, 100.0), 100);
        assert_eq!(percentile(&mut [], 95.0), 0);
        assert_eq!(percentile(&mut [42], 95.0), 42);
    }

    #[test]
    fn b64_span_id_to_hex() {
        // "XWfc0HLIVjM=" → the bytes rendered as hex.
        assert_eq!(b64_to_hex("XWfc0HLIVjM="), "5d67dcd072c85633");
        assert_eq!(b64_to_hex("not-base64!!!"), "not-base64!!!");
    }

    #[test]
    fn attr_str_finds_service_name() {
        let attrs = serde_json::json!([
            {"key":"project","value":{"stringValue":"sideline"}},
            {"key":"service.name","value":{"stringValue":"sideline-server"}}
        ]);
        assert_eq!(
            attr_str(Some(&attrs), "service.name").as_deref(),
            Some("sideline-server")
        );
        assert_eq!(attr_str(Some(&attrs), "missing"), None);
    }
}
