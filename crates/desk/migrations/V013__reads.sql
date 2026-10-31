-- How far the person has read each place of this Desk: a room, a thread, a
-- session, Activity, the brief, the update. `owner` is `you` until rooms
-- are shared. `floor` is the moment Desk began keeping these: older content
-- counts as read.
CREATE TABLE reads (
    owner      TEXT        NOT NULL,
    place      TEXT        NOT NULL,
    at         TIMESTAMPTZ NOT NULL,
    message    TEXT,
    updated_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (owner, place)
);
-- When each session came into its state, kept across restarts.
ALTER TABLE sessions ADD COLUMN state_at TIMESTAMPTZ;
UPDATE sessions SET state_at = updated_at;
