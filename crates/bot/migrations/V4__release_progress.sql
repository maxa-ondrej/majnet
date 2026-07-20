-- Release progress (ADR 0022): a live per-release stepper the dashboard renders,
-- advanced by the bot through committing → tagging → building → published →
-- tracked. Keyed by (org, app, version) with `version` normalized to bare (no
-- leading `v`) so the cut-time key and the webhook-time key match regardless of
-- tag spelling. Best-effort + cosmetic — terminal rows are GC'd after a TTL.
CREATE TABLE IF NOT EXISTS release_progress (
    org TEXT NOT NULL,
    app TEXT NOT NULL,
    version TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',   -- active | done | failed
    stage TEXT NOT NULL,                     -- committing | tagging | building | published | tracked
    detail TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (org, app, version)
);
