-- Self-learning knowledge items — agent-curated, primary-cited facts.
-- Tables live in the `self_learning` schema (see PostgresSelfLearningStore::SCHEMA).
-- Type translations match the SQLite schema in src/sqlite.rs.

CREATE TABLE IF NOT EXISTS self_learning_items (
    id                  UUID             PRIMARY KEY,
    agent_id            TEXT             NOT NULL,
    plan_id             TEXT             NOT NULL,
    topic               TEXT             NOT NULL,
    claim               TEXT             NOT NULL,
    content_hash        TEXT             NOT NULL,
    memory_id           TEXT,
    status              TEXT             NOT NULL DEFAULT 'provisional',
    confidence          REAL             NOT NULL DEFAULT 0,
    high_stakes         BOOLEAN          NOT NULL DEFAULT FALSE,
    source_kind         TEXT             NOT NULL,
    source_skill        TEXT,
    source_reference    TEXT             NOT NULL,
    source_excerpt      TEXT,
    created_at          TIMESTAMPTZ      NOT NULL,
    created_by          TEXT             NOT NULL,
    verified_at         TIMESTAMPTZ      NOT NULL,
    expires_at          TIMESTAMPTZ,
    reviewed_at         TIMESTAMPTZ,
    reviewed_by         TEXT,
    review_note         TEXT,
    superseded_by       UUID,
    UNIQUE (agent_id, content_hash)
);

CREATE INDEX IF NOT EXISTS idx_self_learning_items_agent
    ON self_learning_items(agent_id);
CREATE INDEX IF NOT EXISTS idx_self_learning_items_hash
    ON self_learning_items(content_hash);
CREATE INDEX IF NOT EXISTS idx_self_learning_items_status
    ON self_learning_items(status);
CREATE INDEX IF NOT EXISTS idx_self_learning_items_memory
    ON self_learning_items(memory_id);
CREATE INDEX IF NOT EXISTS idx_self_learning_items_expires
    ON self_learning_items(expires_at);
