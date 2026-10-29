-- What an archived message is in a Desk conversation: its room, thread,
-- speaker and why it was delivered. Additive, so a replica on the previous
-- binary keeps writing rows without it.
ALTER TABLE session_archive ADD COLUMN IF NOT EXISTS desk JSONB;
