-- Append-only event log for the lessons-learned subsystem. Backs the admin
-- Lessons dashboard: corpus growth, reinforcement, cross-day recurrence, and
-- per-lesson prevention rate. Reinforcement/injection were previously only
-- tracing log lines, so improvement-over-time could not be reconstructed.
--
-- `trigger_tags` is a JSON-encoded array of the lesson's trigger handles
-- (without the `trigger:` prefix), stored as TEXT for backend portability —
-- the prevention-rate join needs them (injected -> later same-trigger
-- recurrence). Numeric columns are DOUBLE PRECISION / BIGINT to match the
-- Rust f64 / i64 params bound by tokio_postgres.
CREATE TABLE IF NOT EXISTS lesson_events (
    id                BIGSERIAL PRIMARY KEY,
    timestamp         TIMESTAMPTZ NOT NULL,
    agent_id          TEXT NOT NULL,
    identity_id       TEXT,
    lesson_id         TEXT,
    event_type        TEXT NOT NULL,   -- extracted | injected | reinforced_positive | reinforced_negative | merged_recurrence | pruned | superseded
    learning_kind     TEXT,            -- tool_failure | user_correction | repeated_retry
    recall_score      DOUBLE PRECISION,
    importance_delta  DOUBLE PRECISION,
    confidence_after  DOUBLE PRECISION,
    applied_after     BIGINT,
    session_id        TEXT,
    trigger_tags      TEXT             -- JSON array of trigger handles
);

CREATE INDEX IF NOT EXISTS idx_lesson_events_agent_ts ON lesson_events (agent_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_lesson_events_type_ts ON lesson_events (event_type, timestamp);
