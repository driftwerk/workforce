-- Deep-expertise knowledge tree — synthesized, source-grounded nodes that
-- form a per-plan hierarchy (parent_id NULL at a root). Parallel to
-- self_learning_items. Type translations match the SQLite schema in
-- src/sqlite.rs; `sources`/`coverage` are JSON text (serde_json).

CREATE TABLE IF NOT EXISTS deep_nodes (
    id                  UUID             PRIMARY KEY,
    agent_id            TEXT             NOT NULL,
    plan_id             TEXT             NOT NULL,
    parent_id           UUID,
    depth               INTEGER          NOT NULL DEFAULT 0,
    title               TEXT             NOT NULL,
    path_hash           TEXT             NOT NULL,
    body                TEXT             NOT NULL,
    sources             TEXT             NOT NULL DEFAULT '[]',
    coverage            TEXT             NOT NULL DEFAULT '[]',
    memory_id           TEXT,
    status              TEXT             NOT NULL DEFAULT 'seeded',
    confidence          REAL             NOT NULL DEFAULT 0,
    created_at          TIMESTAMPTZ      NOT NULL,
    created_by          TEXT             NOT NULL,
    verified_at         TIMESTAMPTZ      NOT NULL,
    expires_at          TIMESTAMPTZ,
    reviewed_at         TIMESTAMPTZ,
    reviewed_by         TEXT,
    review_note         TEXT,
    superseded_by       UUID
);

CREATE INDEX IF NOT EXISTS idx_deep_nodes_path
    ON deep_nodes(agent_id, path_hash);
CREATE INDEX IF NOT EXISTS idx_deep_nodes_parent
    ON deep_nodes(agent_id, parent_id);
