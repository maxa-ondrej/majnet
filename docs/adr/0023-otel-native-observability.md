# 0023 — OTEL-native observability (traces + logs)

**Status:** accepted (pre-implementation) · **Date:** 2026-07-21 · relates to [0017](0017-metrics-history-persistence.md), [0021](0021-service-apps.md), [0019](0019-intra-project-service-discovery.md), [0013](0013-auto-assigned-vpn-ingress-hosts-with-ssl.md), [0014](0014-managed-db-access-via-adminer.md)

## Context

MajNet already has **agentless infra telemetry**: the reconciler pulls node +
container metrics straight from the Docker API, persists tiered history (ADR
0017), and drives Discord alerting — all with no per-app cost. What it lacks is
the **application** plane:

- **Traces** — cross-service latency and error attribution (proxy → server →
  postgres). The real gap; impossible to reconstruct from container stats.
- **Structured, searchable, retained logs** — today logs are ephemeral
  `docker logs` streamed live; there is no history, search, or correlation.
- **App-level metrics** — request rate / error rate / latency, vs. only
  CPU/mem.

Two forcing functions:

- Apps in the fleet already emit **OTLP** (e.g. `sideline` set
  `OTEL_EXPORTER_OTLP_ENDPOINT`), but it points at the **dying Coolify
  collector** (`otelcollectorhttp-…majksa.net`) — decommissioning Coolify leaves
  them with nowhere to send telemetry.
- **Service apps (ADR 0021)** now let MajNet run off-the-shelf images
  (collector, datastores, UI) with placement + ingress + volumes and no build
  machinery — which is exactly what an observability backend is.

The design must not weaken the invariants: credential isolation, static
trust-zone placement, nothing DB-/telemetry-sensitive on the public edge.

## Decision

### The load-bearing contract: OTLP at the app boundary

Standardize on **OpenTelemetry (OTLP)**. Apps emit logs/metrics/traces via an
OTEL SDK to an endpoint MajNet provides; they never know the backend. That one
decision makes the **backend swappable** and lets the product choice be deferred
and later changed without touching a single app.

MajNet **auto-injects** the wiring into an opted-in app's container — the same
mechanism that already injects `DATABASE_URL` (the reconciler's per-app
`extra_env` in `converge_one`):

```
OTEL_EXPORTER_OTLP_ENDPOINT = http://otel-collector:4317   # project-network alias
OTEL_RESOURCE_ATTRIBUTES    = service.name=<app>, service.version=<from /info>,
                              deployment.environment=<class>, project=<project>
```

Opt in per app with an `otel: on` field on `AppDecl` (GitOps in `project.yaml`,
dashboard-written, like `release:`). The app only has to add an OTEL SDK — no
endpoint to configure, no secret to manage, resource attributes filled in by the
platform so every signal is already tagged by app/version/env/project.

### Layering — don't duplicate what already works

- **Infra signals stay native/pull** — the reconciler ← Docker (ADR 0017). Kept.
- **App signals go OTEL/push** — traces, app metrics, structured logs.

A shared **OTEL Collector gateway** (a service) is the single indirection point:
sampling, PII redaction, and fan-out to the backends live there, so swapping or
adding a backend is a collector-config change, not a fleet change. Because
project networks are isolated (ADR 0019 aliases are per-network), the reconciler
attaches the collector to each project network — the same pattern used to reach
`majnet-postgres` from the admin network for Adminer (ADR 0014) — so an app
resolves the collector by a stable alias on its own network.

### Backend — Grafana Tempo + Loki, metrics reused

Run **Tempo (traces) + Loki (logs) + Grafana (UI)** as internal **service apps
(ADR 0021)** composed on the project network (ADR 0019). **No metrics TSDB**
(Mimir/ClickHouse): MajNet already has metrics + alerting, and Grafana reads them
alongside — so we add only the two real gaps (traces, logs) and keep the
footprint light. SigNoz was considered and rejected for this reason (its
ClickHouse is a heavier operational commitment than the gaps justify).

The product is recorded, but it is the **replaceable** part; the OTLP boundary +
the native-metrics / OTEL-app-signals split are what this ADR pins.

### Placement — internal, tailnet-only

The backend is **`exposure: internal`** (ADR 0021 → `stable` class → private
node, tailnet auto-host, ADR 0013) — telemetry is sensitive and never touches the
public edge. Grafana is reached over the tailnet like the dashboard; the
dashboard deep-links to it ("Open in Grafana ↗") the way it deep-links Adminer.

### Dashboard surface

A per-app **Observability tab** (app-detail, next to Deploys/Releases, gated on
`otel`): golden-signal tiles (RED from traces + native memory/CPU, each labeled
by source), a Traces⇄Logs panel with an inline span waterfall, and
"Open in Grafana" for deep analysis. Native summary + lists in MajNet; Grafana is
the power-user escape hatch — mirroring how metrics + logs already render
natively today. (UX prototyped as a mock, 2026-07-21; pending sign-off.)

## Consequences

- Apps get first-class traces + retained logs by flipping one switch, and their
  OTLP finally has a home that isn't Coolify.
- The backend is swappable behind the OTLP contract; MajNet is not wedded to
  Tempo/Loki/Grafana forever.
- One more field on `AppDecl` (`otel`) and a new dashboard tab; the reconciler
  gains OTEL env injection + collector-network attachment (small, reuses the
  DATABASE_URL and Adminer-network patterns).
- Reuses the entire service-app pipeline for the backend — no bespoke
  observability deploy path.

## Phasing

1. **App emit-readiness (no infra).** `otel: on` on `AppDecl` + reconciler
   auto-injection of `OTEL_EXPORTER_OTLP_ENDPOINT` + `OTEL_RESOURCE_ATTRIBUTES`.
   Cheap; makes every app emit-ready before a backend exists.
2. **Backend.** Collector + Tempo + Loki + Grafana as internal service apps;
   collector attached to project networks. **Gated on the private node** (still
   parked) + volume placement for the stateful stores (Tempo/Loki) — the same
   dependency as per-project Adminer routes (ADR 0014).
3. **Dashboard.** The Observability tab + "Open in Grafana" deep-links.

## Open questions

- **One shared observability stack vs. per-project.** A shared stack is simpler
  but needs the collector on every project network + per-tenant scoping in
  Grafana; per-project isolates cleanly but multiplies footprint.
- **Retention + volume sizing** for Tempo/Loki; backups.
- **Grafana dashboards + datasources as code** (GitOps-provisioned) vs.
  hand-built.
- **Alerting boundary** — keep infra alerts on the dashboard's Discord path; use
  Grafana alerting only for app/trace-level, or consolidate.
- **Sampling policy** at the collector (head vs. tail; keep-all-errors).
