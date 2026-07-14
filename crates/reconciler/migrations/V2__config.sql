-- Operational key/value config (not git-managed — same class as the bot's
-- ghcr_token): the Discord alert webhook, alert thresholds, and the set of
-- currently-firing alerts (so a reconciler restart doesn't re-alert).
CREATE TABLE IF NOT EXISTS config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
