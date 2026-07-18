-- Draft releases (ADR 0009 follow-up): a review-gated release. Instead of
-- cutting immediately, the bot prepares a draft — the proposed next version and
-- a generated changelog — on each push to an app repo's `main`, and waits for
-- an operator to submit it. Submitting cuts the tag (the existing cut→CI→record
-- flow). Keyed per (org, repo): a monorepo's release is repo-wide, so one draft
-- covers every app sharing the repo.
CREATE TABLE release_drafts (
    org          TEXT NOT NULL,
    repo         TEXT NOT NULL,
    version      TEXT NOT NULL,           -- proposed vX.Y.Z (with the leading v)
    bump         TEXT NOT NULL,           -- patch | minor | major
    base         TEXT NOT NULL DEFAULT '',-- the tag the range was computed from ('' = first release)
    commit_count INTEGER NOT NULL DEFAULT 0,
    notes        TEXT NOT NULL DEFAULT '',-- generated changelog (markdown); operator-editable
    notes_edited INTEGER NOT NULL DEFAULT 0, -- 1 once edited, so a push refresh keeps the operator's notes
    updated_at   TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (org, repo)
);

-- The notes a submitted release carried, kept per (org, app, version) so the
-- Releases list can show each release's changelog. A repo-wide submit writes a
-- row per app in the repo.
CREATE TABLE release_notes (
    org          TEXT NOT NULL,
    app          TEXT NOT NULL,
    version      TEXT NOT NULL,
    notes        TEXT NOT NULL,
    submitted_by TEXT NOT NULL DEFAULT '',
    submitted_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (org, app, version)
);
