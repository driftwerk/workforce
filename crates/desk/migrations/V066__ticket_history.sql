-- A ticket's threads and its issue's history.
--
-- `ticket_comments.parent_remote_id`: the id there of the comment one
-- answers (Linear's replies); none for a top-level comment, a Jira one, or
-- Desk's own.
--
-- `ticket_history`: what happened to a synced ticket's issue there, as last
-- read with its comments (`events`: a JSON array, oldest first, each
-- {at, actor?, by, kind, field?, from?, to?}), and when it was read.
ALTER TABLE ticket_comments ADD COLUMN parent_remote_id TEXT;

CREATE TABLE ticket_history (
    ticket   UUID        PRIMARY KEY REFERENCES tickets (id) ON DELETE CASCADE,
    events   JSONB       NOT NULL,
    read_at  TIMESTAMPTZ NOT NULL
);
