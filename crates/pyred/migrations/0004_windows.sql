-- Window level (Session -> Window -> Pane). Each window owns its own layout
-- tree; the session-level layout column (0002) becomes vestigial (kept for
-- rollback). NULL window layout => single-leaf fallback at load time.
CREATE TABLE IF NOT EXISTS windows (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL,            -- logical FK -> sessions(id) (unenforced, matches panes.session_id)
    name        TEXT NOT NULL DEFAULT '',
    layout      TEXT,                      -- JSON LayoutNode; NULL = no layout yet
    position    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS windows_session_pos ON windows(session_id, position ASC);

-- Pane -> Window assignment. Nullable: backfilled by backfill_windows() for
-- existing rows, and (hybrid mode) assigned lazily when the supervisor first
-- sees a worker pane. Logical FK -> windows(id), unenforced.
ALTER TABLE panes ADD COLUMN window_id TEXT;
