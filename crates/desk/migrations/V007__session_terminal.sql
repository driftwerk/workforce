-- The terminal a session runs in, when it was started from one of Desk's
-- terminals rather than by Desk itself.
ALTER TABLE sessions ADD COLUMN terminal_id UUID;
