-- Backfill empty scope arrays to the wildcard ["*"] so existing CLI- and
-- iOS-paired devices keep working after the hub flips
-- `jwt::has_scope([], _)` from "allow" to "deny" (security finding 4.1.3).
--
-- New tokens issued after the flip must declare scopes explicitly; this
-- migration only covers pre-flip rows. After this runs, no
-- `approved_devices` row should have `scopes = '[]'`.

UPDATE approved_devices
SET scopes = '["*"]'::jsonb
WHERE scopes = '[]'::jsonb;

UPDATE device_pairing_requests
SET scopes = '["*"]'::jsonb
WHERE scopes = '[]'::jsonb;
