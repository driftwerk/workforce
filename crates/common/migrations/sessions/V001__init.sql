-- Sessions store — shared Postgres backend (horizontal-scaling inc1b).
--
-- Two tables, exploiting the asymmetry JsonlSessionStore already relies on:
--   * the live transcript is rewritten whole each turn  -> `sessions.messages` JSONB blob
--   * the archive is append-only, one row per message   -> `session_archive`

-- Live transcript: one row per session. `messages` holds the full live
-- transcript as a JSON array so one UPDATE preserves the exact whole-Session
-- `save` semantics. Scalar metadata that needs to be queried is promoted out
-- of `metadata` into indexed columns (`sealed`, `seq`, `prev_session_id`).
CREATE TABLE IF NOT EXISTS sessions (
    id                 UUID PRIMARY KEY,
    agent_id           TEXT NOT NULL,
    channel            TEXT NOT NULL,
    owner              TEXT NOT NULL,
    label              TEXT,
    kind               TEXT NOT NULL,
    context_type       TEXT,
    created_at         TIMESTAMPTZ NOT NULL,
    updated_at         TIMESTAMPTZ NOT NULL,
    total_tokens       BIGINT NOT NULL DEFAULT 0,
    message_count      INT NOT NULL DEFAULT 0,
    model_override     TEXT,
    compaction_history JSONB NOT NULL DEFAULT '[]'::jsonb,
    metadata           JSONB NOT NULL DEFAULT 'null'::jsonb,
    messages           JSONB NOT NULL DEFAULT '[]'::jsonb,
    -- Promoted from metadata for indexing / the active-session invariant.
    sealed             BOOLEAN NOT NULL DEFAULT FALSE,
    seq                BIGINT NOT NULL DEFAULT 0,
    prev_session_id    UUID,
    -- Optimistic-concurrency version. `save` bumps it under FOR UPDATE; a
    -- writer whose `loaded_revision` no longer matches appends rather than
    -- overwriting, so a concurrent turn is never lost.
    revision           BIGINT NOT NULL DEFAULT 0
);

-- One active (unsealed) session per conversation. Enforces the "newest
-- unsealed member" invariant across instances and resolves the cross-instance
-- get_or_create race together with INSERT ... ON CONFLICT DO NOTHING.
CREATE UNIQUE INDEX IF NOT EXISTS sessions_active_uniq
    ON sessions (agent_id, channel, owner)
    WHERE NOT sealed;

CREATE INDEX IF NOT EXISTS sessions_agent_updated
    ON sessions (agent_id, updated_at DESC);

CREATE INDEX IF NOT EXISTS sessions_prev
    ON sessions (prev_session_id);

-- Append-only display history: one row per message, preserving every turn
-- even after the live transcript is compacted. Enables real LIMIT/cursor
-- pagination and full-text search instead of whole-file scans.
CREATE TABLE IF NOT EXISTS session_archive (
    session_id  UUID NOT NULL,
    seq         BIGSERIAL,
    message_id  TEXT NOT NULL,
    role        TEXT NOT NULL,
    content     TEXT NOT NULL,
    tool_data   JSONB,
    ts          TIMESTAMPTZ NOT NULL,
    tokens      INT NOT NULL DEFAULT 0,
    content_tsv TSVECTOR GENERATED ALWAYS AS (to_tsvector('english', content)) STORED,
    PRIMARY KEY (session_id, seq)
);

CREATE INDEX IF NOT EXISTS session_archive_tsv
    ON session_archive USING GIN (content_tsv);
