-- Admin-defined, team-free custom roles (Postgres backend).
--
-- Translation of the `custom_role` table in
-- `crates/permissions/src/sqlite.rs::SqlitePermissionStore::init_schema`.
-- A role's `key` is the matrix lookup key carried on `identities.role`; its
-- capability cells live in `capability_override` keyed by that same `key`.

CREATE TABLE IF NOT EXISTS custom_role (
    id          UUID PRIMARY KEY,
    key         TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    created_at  TIMESTAMPTZ NOT NULL,
    created_by  UUID
);
