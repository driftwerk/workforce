-- Active-run lease for indexing operations. One row per repo while
-- indexing is in progress; deleted on completion. Survives process
-- restarts so the next worker can detect interruptions via stale
-- heartbeats and resume.

CREATE TABLE IF NOT EXISTS index_lease (
    repo_id              TEXT PRIMARY KEY REFERENCES repositories(id),
    worker_id            TEXT NOT NULL,
    target_commit        TEXT,
    phase                TEXT NOT NULL,
    acquired_at_ms       BIGINT NOT NULL,
    last_heartbeat_at_ms BIGINT NOT NULL,
    entities_total       BIGINT,
    entities_processed   BIGINT,
    summaries_total      BIGINT,
    summaries_processed  BIGINT
);

CREATE INDEX IF NOT EXISTS idx_index_lease_heartbeat
    ON index_lease(last_heartbeat_at_ms);
