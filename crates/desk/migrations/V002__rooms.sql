CREATE TABLE rooms (
    id             UUID PRIMARY KEY,
    slug           TEXT        NOT NULL UNIQUE,
    name           TEXT        NOT NULL,
    members        JSONB       NOT NULL,
    default_member TEXT,
    created_at     TIMESTAMPTZ NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL
);

CREATE TABLE room_messages (
    id         UUID PRIMARY KEY,
    room_id    UUID        NOT NULL REFERENCES rooms (id) ON DELETE CASCADE,
    thread_id  UUID        REFERENCES room_messages (id) ON DELETE CASCADE,
    author     TEXT        NOT NULL,
    content    TEXT        NOT NULL,
    hop        SMALLINT    NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL,
    search     TSVECTOR    GENERATED ALWAYS AS (to_tsvector('english', content)) STORED
);

CREATE INDEX room_messages_timeline ON room_messages (room_id, created_at);
CREATE INDEX room_messages_thread ON room_messages (thread_id, created_at) WHERE thread_id IS NOT NULL;
CREATE INDEX room_messages_search ON room_messages USING GIN (search);
