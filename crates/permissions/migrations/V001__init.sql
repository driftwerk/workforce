-- Permission store schema (Postgres backend).
--
-- Translation of `crates/permissions/src/sqlite.rs::SqlitePermissionStore::init_schema`.
-- All tables live under the schema named when this set is registered
-- (`permissions` by default — see `MigrationSet`). The runner sets
-- `search_path` before applying.

CREATE TABLE IF NOT EXISTS permission_rules (
    id              UUID PRIMARY KEY,
    scope_type      TEXT NOT NULL,
    scope_id        TEXT NOT NULL,
    tool_pattern    TEXT NOT NULL,
    state           TEXT NOT NULL,
    conditions      JSONB,
    origin          TEXT NOT NULL,
    created_by      UUID,
    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL,
    UNIQUE (scope_type, scope_id, tool_pattern)
);

CREATE INDEX IF NOT EXISTS idx_permission_rules_scope
    ON permission_rules(scope_type, scope_id);

CREATE INDEX IF NOT EXISTS idx_permission_rules_tool
    ON permission_rules(tool_pattern);

CREATE TABLE IF NOT EXISTS permission_role (
    id              UUID PRIMARY KEY,
    team_id         UUID NOT NULL,
    name            TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    created_at      TIMESTAMPTZ NOT NULL,
    UNIQUE (team_id, name)
);

CREATE INDEX IF NOT EXISTS idx_permission_role_team
    ON permission_role(team_id);

CREATE TABLE IF NOT EXISTS permission_role_grants (
    role_id         UUID NOT NULL REFERENCES permission_role(id) ON DELETE CASCADE,
    rule_id         UUID NOT NULL REFERENCES permission_rules(id) ON DELETE CASCADE,
    PRIMARY KEY (role_id, rule_id)
);

CREATE TABLE IF NOT EXISTS identity_role_membership (
    identity_id     UUID NOT NULL,
    role_id         UUID NOT NULL REFERENCES permission_role(id) ON DELETE CASCADE,
    granted_by      UUID,
    granted_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (identity_id, role_id)
);

CREATE INDEX IF NOT EXISTS idx_identity_role_membership_identity
    ON identity_role_membership(identity_id);

-- Single-row table holding the monotonic generation counter that the
-- per-process LRU cache compares against on every hit. The CHECK keeps
-- the singleton invariant explicit. Initialised inline so reads never
-- have to insert-on-miss.
CREATE TABLE IF NOT EXISTS permission_rules_generation (
    singleton  INTEGER PRIMARY KEY CHECK (singleton = 0),
    generation BIGINT NOT NULL
);

INSERT INTO permission_rules_generation (singleton, generation)
    VALUES (0, 0)
    ON CONFLICT (singleton) DO NOTHING;
