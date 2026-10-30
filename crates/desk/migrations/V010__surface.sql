-- A room's conversation channel, frozen when the room is made: `desk:<id>`
-- for new rooms, and the slug channel each older room's agents already
-- hold their sessions on, so renaming a room no longer forks them. A room
-- whose slug is `tuple` would share the call's channel, so it takes its id.
ALTER TABLE rooms ADD COLUMN channel TEXT;
UPDATE rooms SET channel = CASE WHEN slug = 'tuple' THEN 'desk:' || id ELSE 'desk:' || slug END;
ALTER TABLE rooms ALTER COLUMN channel SET NOT NULL;
CREATE UNIQUE INDEX rooms_channel ON rooms (channel);

-- Where an agent's room message came from (`<hub>/<agent>/<message id>`,
-- its session message), so posting it again is a no-op; why it was
-- delivered outside a turn (`reminder`, `job`, …) when it was; and the
-- approval an `approval` delivery asks the person to decide.
ALTER TABLE room_messages ADD COLUMN source TEXT, ADD COLUMN delivery TEXT, ADD COLUMN request_id TEXT;
CREATE UNIQUE INDEX room_messages_source ON room_messages (source) WHERE source IS NOT NULL;
