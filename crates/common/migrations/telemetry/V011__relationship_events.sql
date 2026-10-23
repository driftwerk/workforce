-- Per-user relationship tracking. `relationship_events` is the append-only log
-- of scored human DM turns (sentiment, reaction to the prior agent turn, key
-- moments); the rolled-up summary is computed in-process over a time window.
-- `relationship_cursor` is the per-session high-water mark the code-only
-- periodic scan advances so it never re-scores a turn.
--
-- UNIQUE(session_id, message_id) lets the live hook and the periodic scan both
-- attempt a turn idempotently (ON CONFLICT DO NOTHING). Numeric columns are
-- DOUBLE PRECISION to match the Rust f64 params bound by tokio_postgres.
CREATE TABLE IF NOT EXISTS relationship_events (
    id                BIGSERIAL PRIMARY KEY,
    timestamp         TIMESTAMPTZ NOT NULL,
    agent_id          TEXT NOT NULL,
    identity_id       TEXT NOT NULL,
    session_id        TEXT NOT NULL,
    message_id        TEXT NOT NULL,
    valence           DOUBLE PRECISION NOT NULL,
    intensity         DOUBLE PRECISION NOT NULL,
    emotion_label     TEXT,
    reaction          TEXT NOT NULL,   -- praise | gratitude | acceptance | correction | retry | stop | neutral
    is_key_moment     BOOLEAN NOT NULL,
    confidence        DOUBLE PRECISION NOT NULL,
    source            TEXT NOT NULL,   -- code | code+llm
    signals           TEXT NOT NULL,   -- JSON object of raw marker counts
    UNIQUE (session_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_relationship_events_agent_identity_ts ON relationship_events (agent_id, identity_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_relationship_events_identity_ts ON relationship_events (identity_id, timestamp);

CREATE TABLE IF NOT EXISTS relationship_cursor (
    session_id        TEXT PRIMARY KEY,
    agent_id          TEXT NOT NULL,
    last_message_ts   TIMESTAMPTZ NOT NULL,
    last_message_id   TEXT NOT NULL,
    updated_at        TIMESTAMPTZ NOT NULL
);
