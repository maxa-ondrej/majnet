# 0017 — Metrics-history persistence (tiered rollups)

**Status:** accepted · **Date:** 2026-07-18 · relates to [0011](0011-control-plane-schema-migrations.md)

## Context

The dashboard shows node/host CPU/MEM charts, but they only exist *live*: the
reconciler's `metrics::gather` is called on demand by `GET /api/metrics`, and
the `/nodes` page accumulates a rolling ~10-minute window client-side. Navigate
away and the history is gone; there is no way to see the last hour, day, or
week, and the home-dashboard fleet widget can only show a current value.

Persisting metrics needs to (a) not balloon the DB, (b) not add an agent or a
second datastore, and (c) not violate "the reconciler carries no state git
doesn't" beyond the observability data it already keeps (`app_info`, alert
state) — metrics are a cache of what containers reported, not a source of truth.

## Decision

Persist node/host samples in the **reconciler's SQLite** (never git — same class
as `app_info`), with **RRD-style tiered rollups** so the table stays small.

- **Write:** a dedicated `metrics::sample_loop` (spawned beside `alerts::run_loop`)
  calls the existing `gather` every **60s** and writes one raw row per reachable
  node to `metric_samples (ts, node, cpu_pct, mem_used, mem_total, containers_running)`,
  PK `(node, ts)`. Independent of alerting (history is kept whether or not a
  Discord webhook is configured). Runs `gather` a second time per minute
  alongside the alert evaluator — accepted for v1 (cheap; one exec + 1s CPU
  sample per node). Unifying the two gatherers is a future option.
- **Retention (compaction):** every ~15 min the sampler ages rows into coarser,
  time-aligned buckets (averaged), keeping the newest data granular:

  | Age | Resolution | Bucket |
  |---|---|---|
  | ≤ 24h | raw | 60s |
  | 24h–7d | 2/hour | 1800s |
  | 7d–30d | 1/hour | 3600s |
  | > 30d | 1/day | 86400s (kept indefinitely) |

  ~2,300 rows/node for the first month, +365/node/year after. Compaction is
  idempotent: bucket timestamps are `ts/bucket*bucket`, so re-aggregating
  already-bucketed rows re-inserts the same aligned rows (`INSERT OR REPLACE`).
- **Read:** additive `GET /api/metrics/history?range=<sec>&node=<name>` returns
  samples in the window, oldest first, already at the resolution appropriate for
  their age. Live `GET /api/metrics` is unchanged (instantaneous snapshot).
- **UI:** a time-range selector (Live / 1h / 6h / 24h / 7d / 30d) on `/nodes`
  feeds the existing `MetricChart` from history instead of live accumulation;
  the home-dashboard fleet widget renders 6h sparklines.

## Consequences

- Real historical charts with no new services and a bounded, self-pruning table.
- **v1 is node/host-level only.** Per-container history (higher cardinality) is a
  fast-follow — the same table shape with a `container` column.
- No pre-aggregated rollup tables: query-time reads are cheap because compaction
  already bounds row count. At much larger fleets/longer ranges, add explicit
  rollup tables or display-time bucketing.
- Metrics live in the reconciler DB and are **not** backed up via git — acceptable
  for observability data; a node loss loses its history, not platform state.
