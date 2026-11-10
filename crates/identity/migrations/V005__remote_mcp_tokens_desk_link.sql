-- Desk boss links: a token of kind `desk_boss` (its scopes say so, beside
-- the `device:<id>` that minted it) carries what it links here: the agent
-- that bosses, the Desk group, the engine, and the engine's Ed25519 public
-- key. NULL for every other token.
--
-- Additive and nullable, so a previous-release node, which lists its
-- columns by name, neither reads nor writes it, and every existing key
-- stays a remote MCP key. Safe to run more than once.

ALTER TABLE remote_mcp_tokens
    ADD COLUMN IF NOT EXISTS desk_link JSONB;
