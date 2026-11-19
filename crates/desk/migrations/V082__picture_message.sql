-- A picture row that is a session's message to its boss keeps that line
-- message's id: a row cut short says where it is read whole. Additive.
ALTER TABLE picture_events ADD COLUMN IF NOT EXISTS message UUID;
