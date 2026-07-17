-- Node/host metrics history for the dashboard's time-range charts. A sampler
-- loop (crates/reconciler/src/metrics.rs) writes one raw row per reachable node
-- every 60s; a periodic compaction ages rows into coarser, time-aligned buckets
-- so the table stays small (RRD-style tiers, ADR 0017):
--   ≤ 24h  raw 60s   ·   24h–7d  30-min   ·   7d–30d  1h   ·   > 30d  1-day.
-- This is runtime observability, NOT platform state, so it lives here in the
-- reconciler DB (same class as app_info / alert state) and never touches git.
-- The (node, ts) primary key makes aligned-bucket compaction idempotent:
-- re-aggregating already-bucketed rows re-inserts the same aligned timestamps.
CREATE TABLE IF NOT EXISTS metric_samples (
    ts                 INTEGER NOT NULL,   -- unix seconds (bucket-aligned once compacted)
    node               TEXT    NOT NULL,
    cpu_pct            REAL    NOT NULL,
    mem_used           INTEGER NOT NULL,
    mem_total          INTEGER NOT NULL,
    containers_running INTEGER NOT NULL,
    PRIMARY KEY (node, ts)
);
CREATE INDEX IF NOT EXISTS ix_metric_samples_ts ON metric_samples(ts);
