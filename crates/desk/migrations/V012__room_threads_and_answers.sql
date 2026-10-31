-- What a room message answers (an ask, a form, a hand-off), and the tools
-- the agent's turn ran on the way, as its hub reported them.
ALTER TABLE room_messages
  ADD COLUMN in_reply_to UUID REFERENCES room_messages (id) ON DELETE SET NULL,
  ADD COLUMN steps JSONB NOT NULL DEFAULT '[]';
CREATE INDEX room_messages_in_reply_to ON room_messages (in_reply_to) WHERE in_reply_to IS NOT NULL;
-- Pages are ordered by time, then id, so two messages at one instant keep
-- their order across pages.
CREATE INDEX room_messages_place ON room_messages (room_id, thread_id, created_at, id);
