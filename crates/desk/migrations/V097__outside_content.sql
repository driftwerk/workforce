-- What outside content reached a session, as Desk shares its work with
-- agents: what a line message carried in (a boss's turn that had read
-- outside content, a ticket others wrote), and what a session was started
-- on. NULL: none.
ALTER TABLE line_messages ADD COLUMN outside TEXT;
-- A boss's message from before Desk said what it carried is taken as
-- carrying some.
UPDATE line_messages SET outside = 'a boss''s message from before Desk said what it carried'
    WHERE sender ->> 'kind' = 'manager';

CREATE TABLE session_starts (
    session UUID PRIMARY KEY,
    outside TEXT
);
