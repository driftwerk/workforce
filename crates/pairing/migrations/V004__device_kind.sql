-- Pairing — record what kind of device is being paired.
--
-- `Identity.linked_ids` maps a *platform* to one id ("slack" -> …,
-- "ios_device" -> …). Pairing hardcoded `ios_device` for every device, so
-- pairing a second device against the same identity overwrote the first and
-- silently unlinked a device still in use — hit when adding a voice satellite
-- alongside an iPhone.
--
-- The kind captured here becomes the `linked_ids` key on approval, so distinct
-- surfaces coexist on one identity. NULL means `ios_device`: every client that
-- predates this column was an iOS device, so existing rows and older clients
-- keep exactly the behaviour they have now.
ALTER TABLE device_pairing_requests
    ADD COLUMN IF NOT EXISTS device_kind TEXT;
