-- Who said a message in a Desk DM, when the transcript's role doesn't say:
-- a turn the overseer sent an agent, or one the person sent by saying yes to
-- a suggestion. By message; `author` is {"kind", "id", "name"}.
CREATE TABLE message_authors (
    message_id TEXT        PRIMARY KEY,
    agent_id   TEXT        NOT NULL,
    author     JSONB       NOT NULL,
    at         TIMESTAMPTZ NOT NULL
);
