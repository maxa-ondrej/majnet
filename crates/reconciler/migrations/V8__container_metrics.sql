-- Per-container metrics history (ADR 0017 follow-up). Same sampler + tiered
-- compaction as metric_samples, but keyed by (node, container) so the dashboard
-- can chart a single app's CPU/MEM over time. Higher cardinality than the
-- node-level table (rows × containers), but the same RRD tiers keep it bounded.
-- Runtime observability, not platform state — reconciler DB only, never git.
CREATE TABLE IF NOT EXISTS container_samples (
    ts        INTEGER NOT NULL,   -- unix seconds (bucket-aligned once compacted)
    node      TEXT    NOT NULL,
    container TEXT    NOT NULL,
    cpu_pct   REAL    NOT NULL,
    mem_used  INTEGER NOT NULL,
    mem_limit INTEGER NOT NULL,
    PRIMARY KEY (node, container, ts)
);
CREATE INDEX IF NOT EXISTS ix_container_samples ON container_samples(container, ts);
