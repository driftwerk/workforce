-- Translation of `crates/projects/src/sqlite.rs::SqliteProjectStore::init_schema`.
-- Tables live in the schema named when this set is registered (`projects` by
-- default — see `MigrationSet` in `crates/postgres/src/migrations.rs`); the
-- migration runner sets `search_path` before applying.
--
-- Type translations from SQLite:
--   TEXT NOT NULL DEFAULT '[]' (json)  →  JSONB NOT NULL DEFAULT '[]'::jsonb
--   TEXT NOT NULL DEFAULT '{}' (json)  →  JSONB NOT NULL DEFAULT '{}'::jsonb
--   TEXT (RFC3339 timestamp)           →  TIMESTAMPTZ

CREATE TABLE IF NOT EXISTS projects (
    agent_id    TEXT        NOT NULL,
    name        TEXT        NOT NULL,
    repos       JSONB       NOT NULL DEFAULT '[]'::jsonb,
    defaults    JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (agent_id, name)
);

CREATE INDEX IF NOT EXISTS idx_projects_agent ON projects(agent_id);

-- GIN index on repos so future cross-project alias lookups
-- (`repos @> '[{"alias": "rails"}]'::jsonb`) hit an index instead of a full
-- table scan.
CREATE INDEX IF NOT EXISTS idx_projects_repos
    ON projects USING GIN (repos);
