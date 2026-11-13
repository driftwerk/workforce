-- A ticket's comments: its Jira or Linear issue's, as last read when the
-- ticket was opened, and Desk's own (`remote_id` null): the person's on a
-- ticket of their own, and the notes sessions and agents leave, which stay
-- in Desk.
--
-- `author`: who wrote it, as the comment says ({kind, name, on}).
-- `posted_by`: the party in Desk that posted it, when Desk did; it
-- outranks what the comment says, and a read of the issue keeps it.
CREATE TABLE ticket_comments (
    id         UUID        PRIMARY KEY,
    ticket     UUID        NOT NULL REFERENCES tickets (id) ON DELETE CASCADE,
    remote_id  TEXT,
    author     JSONB       NOT NULL,
    posted_by  JSONB,
    body       TEXT        NOT NULL,
    flagged    BOOLEAN     NOT NULL DEFAULT FALSE,
    at         TIMESTAMPTZ NOT NULL,
    edited_at  TIMESTAMPTZ,
    UNIQUE (ticket, remote_id)
);
CREATE INDEX ticket_comments_ticket ON ticket_comments (ticket, at);
