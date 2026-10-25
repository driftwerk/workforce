-- Identity lifecycle operations (see docs/identity-lifecycle-design.md).
--
-- Translation of the `identity_lifecycle_ops` table in
-- `crates/identity/src/sqlite.rs::SqliteIdentityStore::init_schema`. One row
-- per merge, transfer or delete: the audit record, and the per-subject lease
-- that keeps two replicas from operating on one identity at once. No foreign
-- key to `identities`: the record outlives the identity it describes.

CREATE TABLE IF NOT EXISTS identity_lifecycle_ops (
    id           UUID PRIMARY KEY,
    subject      UUID NOT NULL,
    kind         TEXT NOT NULL,
    target       UUID,
    actor        TEXT NOT NULL,
    names        JSONB NOT NULL DEFAULT '[]'::jsonb,
    status       TEXT NOT NULL,
    report       JSONB NOT NULL DEFAULT '{}'::jsonb,
    started_at   TIMESTAMPTZ NOT NULL,
    lease_until  TIMESTAMPTZ NOT NULL,
    finished_at  TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_identity_lifecycle_ops_subject
    ON identity_lifecycle_ops(subject, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_identity_lifecycle_ops_started
    ON identity_lifecycle_ops(started_at DESC);
