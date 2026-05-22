-- M7-C: add per-session layout column (ADR-0005).
-- NULL means "no layout persisted yet"; callers fall back to a single-leaf
-- layout constructed from the session's first pane at load time.
ALTER TABLE sessions ADD COLUMN layout TEXT;
