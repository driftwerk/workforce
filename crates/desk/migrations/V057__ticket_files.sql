-- A ticket's files: its Jira or Linear issue's attachments and uploads,
-- which the pull keeps in step, and the files the person drops on a ticket
-- of their own.
--
-- `source`: 'upload' | 'jira' | 'linear_upload' | 'linear_link'.
-- `remote_id`: what the issue calls it (a Jira attachment's id, a Linear
-- attachment's id or an upload's address); null for an upload.
-- `bytes`: an upload's own bytes; for a remote file, a copy kept since it
-- was last read (`cached_at`), let go of past the cache's cap. A link has
-- none. `thumb_bytes`: the small picture Jira keeps of an attachment.
CREATE TABLE ticket_files (
    id          UUID        PRIMARY KEY,
    ticket      UUID        NOT NULL REFERENCES tickets (id) ON DELETE CASCADE,
    source      TEXT        NOT NULL,
    remote_id   TEXT,
    name        TEXT        NOT NULL,
    media_type  TEXT        NOT NULL,
    size        BIGINT,
    url         TEXT,
    thumb       BOOLEAN     NOT NULL DEFAULT FALSE,
    position    INTEGER     NOT NULL,
    bytes       BYTEA,
    thumb_bytes BYTEA,
    cached_at   TIMESTAMPTZ,
    added_by    JSONB       NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL,
    UNIQUE (ticket, source, remote_id)
);
CREATE INDEX ticket_files_ticket ON ticket_files (ticket, position);
CREATE INDEX ticket_files_cached ON ticket_files (cached_at) WHERE cached_at IS NOT NULL;
