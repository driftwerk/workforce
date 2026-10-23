CREATE TABLE vault_keys (
    key_id     UUID PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE vault_key_wraps (
    wrap_id    UUID PRIMARY KEY,
    key_id     UUID NOT NULL REFERENCES vault_keys (key_id) ON DELETE CASCADE,
    kdf_salt   BYTEA NOT NULL,
    kdf_m_kib  INTEGER NOT NULL,
    kdf_t      INTEGER NOT NULL,
    kdf_p      INTEGER NOT NULL,
    nonce      BYTEA NOT NULL,
    wrapped    BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE vault_keyring (
    singleton     BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    active_key_id UUID NOT NULL REFERENCES vault_keys (key_id)
);

CREATE TABLE vault_scopes (
    scope         TEXT PRIMARY KEY,
    generation    BIGINT NOT NULL DEFAULT 0,
    imported_at   TIMESTAMPTZ,
    import_sha256 BYTEA,
    frozen_at     TIMESTAMPTZ
);

CREATE TABLE vault_entries (
    scope      TEXT NOT NULL REFERENCES vault_scopes (scope),
    path_mac   BYTEA NOT NULL,
    key_id     UUID NOT NULL REFERENCES vault_keys (key_id),
    nonce      BYTEA NOT NULL,
    ciphertext BYTEA NOT NULL,
    PRIMARY KEY (scope, path_mac)
);

CREATE TABLE vault_audit (
    audit_id BIGSERIAL PRIMARY KEY,
    scope    TEXT NOT NULL REFERENCES vault_scopes (scope),
    at       TIMESTAMPTZ NOT NULL,
    key_id   UUID NOT NULL REFERENCES vault_keys (key_id),
    nonce    BYTEA NOT NULL,
    details  BYTEA NOT NULL
);

CREATE INDEX vault_audit_scope_at ON vault_audit (scope, at);
CREATE INDEX vault_audit_at ON vault_audit (at);
