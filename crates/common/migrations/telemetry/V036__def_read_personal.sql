-- A definition saved in a conversation that read the person's bank (DW-719)
-- stays theirs: once set, it is never cleared, and the row is never shared
-- with a team or everyone.
ALTER TABLE workflow_defs ADD COLUMN IF NOT EXISTS read_personal BOOLEAN NOT NULL DEFAULT FALSE;
