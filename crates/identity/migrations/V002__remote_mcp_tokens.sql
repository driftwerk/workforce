-- Remote MCP bearer tokens.
--
-- Translation of the `remote_mcp_tokens` table in
-- `crates/identity/src/sqlite.rs::SqliteIdentityStore::init_schema`.
-- Only the blake3 hash of a token is stored; `token_prefix` exists so a
-- listing can name a token without revealing it.

CREATE TABLE IF NOT EXISTS remote_mcp_tokens (
    id            UUID PRIMARY KEY,
    identity_id   UUID NOT NULL REFERENCES identities(id) ON DELETE CASCADE,
    label         TEXT NOT NULL,
    token_hash    TEXT NOT NULL UNIQUE,
    token_prefix  TEXT NOT NULL,
    scopes        JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_at    TIMESTAMPTZ NOT NULL,
    expires_at    TIMESTAMPTZ NOT NULL,
    last_used_at  TIMESTAMPTZ,
    revoked_at    TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_remote_mcp_tokens_identity
    ON remote_mcp_tokens(identity_id);
CREATE INDEX IF NOT EXISTS idx_remote_mcp_tokens_created_at
    ON remote_mcp_tokens(created_at DESC);
