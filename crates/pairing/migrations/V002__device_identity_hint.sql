-- Pairing — auto-link device → identity on approval.
--
-- Captures a caller-supplied identity hint (email or identity UUID) at
-- pairing-initiation time. The gateway uses it on approval to look up the
-- corresponding identity and attach the new device's `id` as a linked id —
-- the same auto-match flow Slack and Keycloak/OIDC already use. Removes the
-- need for a separate JWT login step on the iOS client.
ALTER TABLE device_pairing_requests
    ADD COLUMN IF NOT EXISTS identity_hint TEXT;
