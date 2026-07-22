-- Per-deploy stage tracking (deploy trackability). The reconciler advances a row
-- through pulling → migrating → starting → health → finalizing as it rolls an app
-- out (blue-green, deploy.rs), so the dashboard can show WHERE a deploy is — and
-- which stage failed — instead of a single opaque converged/FAILED event.
--
-- One row per (project, app, class), overwritten on each rollout, so the table is
-- naturally bounded by the fleet size (no GC needed). Only written on an actual
-- rollout — a steady-state "in sync" reconcile touches nothing. Runtime state,
-- never git.
CREATE TABLE IF NOT EXISTS deploy_progress (
    project    TEXT    NOT NULL,
    app        TEXT    NOT NULL,
    class      TEXT    NOT NULL,
    stage      TEXT    NOT NULL,   -- pulling|migrating|starting|health|finalizing|deployed
    status     TEXT    NOT NULL,   -- active|done|failed
    detail     TEXT    NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL,   -- unix seconds
    PRIMARY KEY (project, app, class)
);
