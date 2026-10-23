-- Pairing store schema (Postgres backend, Phase 4).
--
-- Translation of `crates/pairing/src/store.rs::SqlitePairingStore::migrate`,
-- consolidated to the post-ALTER shape so a fresh deployment gets the
-- complete schema in one migration. Subsequent schema changes ship as
-- `V00n__*.sql` files alongside this one.
--
-- All tables live in the schema named when this set is registered
-- (`pairing` by default — see `MigrationSet` in
-- `crates/postgres/src/migrations.rs`). The migration runner sets
-- `search_path` before applying.
--
-- Type translations from SQLite:
--   TEXT (uuid stringified)            →  TEXT  (we keep these as TEXT
--                                        because the application code
--                                        already treats `id` as a
--                                        platform-controlled string,
--                                        and migration tooling needs
--                                        to preserve the original
--                                        values verbatim)
--   TEXT (RFC3339 timestamp)           →  TIMESTAMPTZ
--   INTEGER (bool 0/1)                 →  BOOLEAN
--   TEXT (json arrays)                 →  JSONB

CREATE TABLE IF NOT EXISTS channel_pairing_requests (
    id              TEXT PRIMARY KEY,
    code            TEXT NOT NULL UNIQUE,
    platform        TEXT NOT NULL,
    channel_id      TEXT NOT NULL,
    sender_id       TEXT NOT NULL,
    sender_name     TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending',
    created_at      TIMESTAMPTZ NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL,
    approved_by     TEXT,
    approved_at     TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_channel_requests_status
    ON channel_pairing_requests(status);

CREATE TABLE IF NOT EXISTS channel_allowlist (
    id              TEXT PRIMARY KEY,
    platform        TEXT NOT NULL,
    channel_id      TEXT NOT NULL,
    sender_id       TEXT NOT NULL,
    sender_name     TEXT NOT NULL,
    added_at        TIMESTAMPTZ NOT NULL,
    added_by        TEXT NOT NULL,
    UNIQUE (platform, sender_id)
);
CREATE INDEX IF NOT EXISTS idx_channel_allowlist_lookup
    ON channel_allowlist(platform, sender_id);

CREATE TABLE IF NOT EXISTS device_pairing_requests (
    id              TEXT PRIMARY KEY,
    code            TEXT NOT NULL UNIQUE,
    device_name     TEXT NOT NULL,
    public_key      TEXT,
    scopes          JSONB NOT NULL DEFAULT '[]'::jsonb,
    status          TEXT NOT NULL DEFAULT 'pending',
    created_at      TIMESTAMPTZ NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL,
    approved_by     TEXT,
    approved_at     TIMESTAMPTZ,
    pending_token   TEXT,
    client_id       TEXT
);

CREATE TABLE IF NOT EXISTS approved_devices (
    id                  TEXT PRIMARY KEY,
    device_name         TEXT NOT NULL,
    scopes              JSONB NOT NULL DEFAULT '[]'::jsonb,
    token_hash          TEXT NOT NULL UNIQUE,
    created_at          TIMESTAMPTZ NOT NULL,
    last_used           TIMESTAMPTZ,
    revoked             BOOLEAN NOT NULL DEFAULT FALSE,
    expires_at          TIMESTAMPTZ,
    client_id           TEXT,
    -- Rotation grace period: when a token is rotated the previous
    -- hash is parked here so a client that never received the
    -- rotation response can still authenticate. Cleared on the next
    -- successful rotation.
    prev_token_hash     TEXT
);
CREATE INDEX IF NOT EXISTS idx_device_token_hash
    ON approved_devices(token_hash);
CREATE INDEX IF NOT EXISTS idx_device_prev_token_hash
    ON approved_devices(prev_token_hash) WHERE prev_token_hash IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_device_client_id
    ON approved_devices(client_id) WHERE client_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS node_pairing_requests (
    id              TEXT PRIMARY KEY,
    code            TEXT NOT NULL UNIQUE,
    node_id         TEXT NOT NULL,
    node_name       TEXT NOT NULL,
    node_type       TEXT NOT NULL,
    capabilities    JSONB NOT NULL DEFAULT '[]'::jsonb,
    status          TEXT NOT NULL DEFAULT 'pending',
    created_at      TIMESTAMPTZ NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL,
    approved_by     TEXT,
    approved_at     TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_node_requests_node_id
    ON node_pairing_requests(node_id, status);
