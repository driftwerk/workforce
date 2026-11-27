-- What outside content reached a session, as Desk shares its work with
-- agents: what a line message carried in (a boss's turn that had read
-- outside content, a ticket others wrote), what a prompt a boss or an
-- agent typed in carried, what a session was started on, and what a
-- boss's turn had read when it wrote a ticket or a comment. NULL: none.
ALTER TABLE line_messages ADD COLUMN outside TEXT;
-- A boss's message to a session from before Desk said what it carried is
-- taken as carrying some: the turn that sent it was never recorded.
UPDATE line_messages SET outside = 'a boss''s message from before Desk said what it carried'
    WHERE sender ->> 'kind' = 'manager' AND recipient ->> 'kind' = 'session';

CREATE TABLE session_starts (
    session UUID PRIMARY KEY,
    outside TEXT
);

-- A prompt a boss or an agent typed into a session, by the marker it
-- went in with.
CREATE TABLE typed_prompts (
    id UUID PRIMARY KEY,
    session UUID NOT NULL,
    sender JSONB NOT NULL,
    outside TEXT,
    at TIMESTAMPTZ NOT NULL
);
CREATE INDEX typed_prompts_session ON typed_prompts (session, at);

-- What a boss's turn had read when it wrote a ticket (by its id) or a
-- comment (by its id): a row with NULL, nothing. One a boss wrote with no
-- row is from before Desk said.
CREATE TABLE written_outside (
    id UUID PRIMARY KEY,
    outside TEXT
);

-- An exchange an agent was sent before Desk said what it carries in is
-- sent once more, saying it; every one sent from now on says it.
ALTER TABLE knows_sent ADD COLUMN outside_said BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE knows_sent ALTER COLUMN outside_said SET DEFAULT true;
