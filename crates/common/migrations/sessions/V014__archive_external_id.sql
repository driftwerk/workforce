-- The client's own id for an archived message (Desk's id for what the
-- person sent), so a Desk conversation's read can say which stored row a
-- message it showed became. Additive, so a replica on the previous binary
-- keeps writing rows without it.
ALTER TABLE session_archive ADD COLUMN IF NOT EXISTS external_id TEXT;
