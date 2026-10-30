-- Files in Desk conversations, by the artifact that holds their bytes: what
-- an agent's turn delivered into a DM or room session, and what the person
-- uploaded to one. `message_id` is the DM message a file rides on once its
-- turn has ended; an agent's file left without one is a message of its own.
-- A person's file is shown once it is on one of their messages.
CREATE TABLE attachments (
    artifact_id  TEXT        PRIMARY KEY,
    agent_id     TEXT        NOT NULL,
    channel      TEXT        NOT NULL,
    owner        TEXT        NOT NULL,
    session_id   TEXT        NOT NULL,
    origin       TEXT        NOT NULL,
    message_id   TEXT,
    media_type   TEXT        NOT NULL,
    filename     TEXT        NOT NULL,
    size         BIGINT      NOT NULL,
    width        INTEGER,
    height       INTEGER,
    duration     DOUBLE PRECISION,
    thumbnail_id TEXT,
    caption      TEXT,
    at           TIMESTAMPTZ NOT NULL
);

CREATE INDEX attachments_by_message ON attachments (message_id) WHERE message_id IS NOT NULL;
CREATE INDEX attachments_by_conversation ON attachments (agent_id, channel, owner, at);
CREATE INDEX attachments_unbound ON attachments (session_id, at) WHERE message_id IS NULL;

-- A room message's files, as Desk posts them: each names the hub and agent
-- whose store holds its bytes.
ALTER TABLE room_messages ADD COLUMN attachments JSONB NOT NULL DEFAULT '[]';
