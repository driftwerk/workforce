-- The boss's one live picture (designs/desk-boss-one-picture.md): what
-- happened that a boss's next instruction to a session stands on, in one
-- ordered feed. `seq` orders it; a session's version is the newest row
-- touching it. Additive.
CREATE TABLE IF NOT EXISTS picture_events (
    seq BIGINT PRIMARY KEY,
    at TIMESTAMPTZ NOT NULL,
    what TEXT NOT NULL,
    boss TEXT,
    session UUID,
    sessions UUID[] NOT NULL DEFAULT '{}',
    text TEXT NOT NULL,
    turn TEXT
);
CREATE INDEX IF NOT EXISTS picture_events_at ON picture_events (at);
