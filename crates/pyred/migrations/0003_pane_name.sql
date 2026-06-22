-- Add optional human-readable name to panes.
-- NULL means "no name set"; clients fall back to a generated label.
ALTER TABLE panes ADD COLUMN name TEXT;
