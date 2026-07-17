-- Interactive terminal session audit (ADR 0016). One row per session; the full
-- I/O transcript is written to a file under data_dir/transcripts/<id>.log (a
-- TTY echoes typed input, so the output stream captures the whole session).
CREATE TABLE terminal_sessions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    actor       TEXT NOT NULL,          -- resolved platform-admin (github login)
    node        TEXT NOT NULL,
    mode        TEXT NOT NULL,          -- 'host' | 'container'
    target      TEXT NOT NULL,          -- node name (host) or project/app/class (container)
    started_at  TEXT NOT NULL DEFAULT (datetime('now')),
    ended_at    TEXT,
    bytes       INTEGER                 -- transcript size, set on close
);
