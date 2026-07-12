-- Runtime key/value config set from the dashboard (ADR 0012): the GHCR pull
-- token lives here so it's settable in Settings without a redeploy. Secrets at
-- rest are the bot's domain (it already holds the GitHub App key).
CREATE TABLE config (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
