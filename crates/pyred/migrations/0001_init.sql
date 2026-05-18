CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    last_active_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS panes (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    argv TEXT NOT NULL DEFAULT '',
    cwd TEXT,
    cols INTEGER NOT NULL,
    rows INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    closed_at INTEGER
);

CREATE TABLE IF NOT EXISTS blocks (
    id TEXT PRIMARY KEY,
    pane_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    command TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    exit_code INTEGER,
    cwd TEXT,
    stdout_blob_path TEXT NOT NULL,
    stdout_len INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS blocks_session_started
    ON blocks(session_id, started_at DESC);
