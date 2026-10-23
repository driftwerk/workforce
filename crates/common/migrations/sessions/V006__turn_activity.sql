-- Turn lifecycle record, promoted out of `metadata`.
--
-- These are columns, not JSON keys, and never bump `revision`. Note the promotion covers the three
-- columns only — the transitional legacy drain in `end_turn` still writes
-- `metadata`, which IS inside the versioned set.
ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS turn_instance   TEXT,
    ADD COLUMN IF NOT EXISTS turn_started_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS turn_message_id TEXT;

CREATE INDEX IF NOT EXISTS sessions_turn_active
    ON sessions (agent_id)
    WHERE turn_instance IS NOT NULL;

-- Transitional: the sweep also reads pre-existing `task_active` markers for
-- one release, so a turn interrupted by an older instance during the rolling
-- deploy that ships this migration is still recovered. The sweep's predicate
-- leads with `metadata ? 'task_active'` so this index is actually used; drop
-- both with that read.
CREATE INDEX IF NOT EXISTS sessions_legacy_turn_marker
    ON sessions (agent_id)
    WHERE metadata ? 'task_active';
