-- Translation of `crates/mcp-store/src/sqlite.rs::SqliteMcpServerStore::init_schema`.
-- Tables live in the schema named when this set is registered (`mcp_store` by
-- default — see `MigrationSet` in `crates/postgres/src/migrations.rs`); the
-- migration runner sets `search_path` before applying.
--
-- Type translations from SQLite:
--   TEXT NOT NULL DEFAULT '{}' (json)  →  JSONB NOT NULL DEFAULT '{}'::jsonb
--   TEXT (RFC3339 timestamp)           →  TIMESTAMPTZ
--
-- Composite primary key (agent_id, server_id) — same per-agent
-- namespacing convention as `projects` / `identities`.

CREATE TABLE IF NOT EXISTS mcp_servers (
    agent_id      TEXT        NOT NULL,
    server_id     TEXT        NOT NULL,
    config        JSONB       NOT NULL DEFAULT '{}'::jsonb,
    installed_via TEXT        NOT NULL DEFAULT 'agent',
    created_at    TIMESTAMPTZ NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (agent_id, server_id)
);

CREATE INDEX IF NOT EXISTS idx_mcp_servers_agent ON mcp_servers(agent_id);

-- GIN index on config so future "all servers using OAuth provider X"
-- filters (`config @> '{"auth": {"provider": "linear"}}'::jsonb`) hit
-- an index instead of a full table scan.
CREATE INDEX IF NOT EXISTS idx_mcp_servers_config
    ON mcp_servers USING GIN (config);
