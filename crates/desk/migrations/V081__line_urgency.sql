-- How urgently a boss sent a line message to a session that was working
-- (design note §4): NULL for every other message. 'order' is an urgent
-- order, counted against the session's urgent limit; 'stand_down' a
-- stand-down, not counted; 'escalate' a stand-down of an irreversible
-- assignment, which interrupts the turn when it isn't read in time. Kept so
-- a restart before the steer was taken steers it again.
ALTER TABLE line_messages ADD COLUMN urgency TEXT
    CHECK (urgency IN ('order', 'stand_down', 'escalate'));
CREATE INDEX line_urgent ON line_messages ((recipient ->> 'id'), at)
    WHERE urgency IS NOT NULL;
