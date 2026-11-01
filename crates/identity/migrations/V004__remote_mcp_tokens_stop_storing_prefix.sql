-- Remote MCP keys no longer keep any part of the key in the clear.
--
-- `token_prefix` held the first characters of each key body for display. The
-- listing now shows the key's name, creation date and a fingerprint derived
-- from `token_hash`, so nothing reads or writes the column any more.
--
-- Rolling deploys: hubs still running the previous release read this column
-- into a non-optional string on every token lookup, including
-- authentication, and write it on mint. So the column is NOT dropped and NOT
-- made NULL here: it gets an empty default and every stored value is
-- overwritten with the empty string. Old nodes keep decoding and inserting
-- it; new nodes ignore it. Drop the column in a later release, once no
-- previous-release node can be running.
--
-- Safe to run more than once. `token_hash`, the only thing authentication
-- uses, is not touched, so existing keys keep working.

ALTER TABLE remote_mcp_tokens
    ALTER COLUMN token_prefix SET DEFAULT '';

UPDATE remote_mcp_tokens
   SET token_prefix = ''
 WHERE token_prefix <> '';
