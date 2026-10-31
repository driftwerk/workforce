-- Where in Desk an event happened (a room, its thread) and the room message
-- it is about, so a row opens it.
ALTER TABLE activity ADD COLUMN place JSONB, ADD COLUMN message UUID;
