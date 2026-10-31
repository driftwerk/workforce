-- A thread message posted to the room's timeline: one copy per message.
ALTER TABLE room_messages ADD COLUMN copy_of UUID REFERENCES room_messages (id) ON DELETE SET NULL;
CREATE UNIQUE INDEX room_messages_copy_of ON room_messages (copy_of) WHERE copy_of IS NOT NULL;
