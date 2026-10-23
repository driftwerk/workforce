-- Identity store schema (Postgres backend, Phase 4).
--
-- Translation of `crates/identity/src/sqlite.rs::SqliteIdentityStore::init_schema`.
-- All tables live in the schema named when this set is registered
-- (`identity` by default — see `MigrationSet` in
-- `crates/postgres/src/migrations.rs`). The migration runner sets
-- `search_path` before applying.
--
-- Type translations from SQLite:
--   TEXT (uuid stringified)            →  UUID
--   TEXT (RFC3339 timestamp)           →  TIMESTAMPTZ
--   TEXT NOT NULL DEFAULT '{}' (json)  →  JSONB NOT NULL DEFAULT '{}'::jsonb
--   TEXT NOT NULL DEFAULT '[]' (json)  →  JSONB NOT NULL DEFAULT '[]'::jsonb

CREATE TABLE IF NOT EXISTS identities (
    id                  UUID PRIMARY KEY,
    display_name        TEXT NOT NULL,
    email               TEXT,
    password_hash       TEXT,
    role                TEXT NOT NULL DEFAULT 'member',
    status              TEXT NOT NULL DEFAULT 'active',
    provisioning_source TEXT NOT NULL DEFAULT 'manual',
    linked_ids          JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at          TIMESTAMPTZ NOT NULL,
    updated_at          TIMESTAMPTZ NOT NULL
);

-- Mirror the SQLite partial unique index: emails are case-sensitive
-- unique when present, but multiple identities can have NULL email.
CREATE UNIQUE INDEX IF NOT EXISTS idx_identities_email
    ON identities(email) WHERE email IS NOT NULL;

-- GIN index on linked_ids so platform-keyed lookups
-- (`linked_ids @> '{"slack": "U0ABC"}'::jsonb`) hit an index instead
-- of a full table scan. The SQLite version uses `LIKE '%...%'` which
-- is unindexable; this is a strict improvement.
CREATE INDEX IF NOT EXISTS idx_identities_linked_ids
    ON identities USING GIN (linked_ids);

CREATE INDEX IF NOT EXISTS idx_identities_role ON identities(role);
CREATE INDEX IF NOT EXISTS idx_identities_status ON identities(status);
CREATE INDEX IF NOT EXISTS idx_identities_created_at ON identities(created_at DESC);

CREATE TABLE IF NOT EXISTS teams (
    id              UUID PRIMARY KEY,
    name            TEXT NOT NULL,
    creator_id      UUID NOT NULL,
    members         JSONB NOT NULL DEFAULT '[]'::jsonb,
    join_requests   JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL
);

-- GIN index on members so `teams_for_member` (where the SQLite version
-- does `LIKE '%uuid%'`) becomes an indexed
-- `members @> '["uuid"]'::jsonb` query.
CREATE INDEX IF NOT EXISTS idx_teams_members
    ON teams USING GIN (members);

CREATE INDEX IF NOT EXISTS idx_teams_creator ON teams(creator_id);
CREATE INDEX IF NOT EXISTS idx_teams_created_at ON teams(created_at DESC);
