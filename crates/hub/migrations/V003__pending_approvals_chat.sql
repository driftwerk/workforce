-- Whether a person's message started the turn that asked: the turn resumed
-- on its decision then runs the conversation model too. Rows from before
-- resume as they always did.
ALTER TABLE pending_approvals ADD COLUMN IF NOT EXISTS chat BOOLEAN NOT NULL DEFAULT FALSE;
