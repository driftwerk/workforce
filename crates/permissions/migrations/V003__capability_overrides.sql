-- Admin-editable capability matrix overrides (Postgres backend).
--
-- Translation of the capability tables in
-- `crates/permissions/src/sqlite.rs::SqlitePermissionStore::init_schema`.
-- Lives in the `permissions` schema alongside the rule tables.

CREATE TABLE IF NOT EXISTS capability_override (
    role        TEXT NOT NULL,
    capability  TEXT NOT NULL,
    decision    TEXT NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL,
    updated_by  UUID,
    PRIMARY KEY (role, capability)
);

CREATE TABLE IF NOT EXISTS capability_override_audit (
    id            UUID PRIMARY KEY,
    at            TIMESTAMPTZ NOT NULL,
    actor_id      UUID,
    role          TEXT NOT NULL,
    capability    TEXT NOT NULL,
    old_decision  TEXT,
    new_decision  TEXT
);

CREATE INDEX IF NOT EXISTS idx_capability_override_audit_at
    ON capability_override_audit(at DESC);

CREATE TABLE IF NOT EXISTS capability_override_generation (
    singleton  INTEGER PRIMARY KEY CHECK (singleton = 0),
    generation BIGINT NOT NULL
);

INSERT INTO capability_override_generation (singleton, generation)
    VALUES (0, 0)
    ON CONFLICT (singleton) DO NOTHING;
