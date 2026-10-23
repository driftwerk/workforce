-- Named byte streams attached to a document, for the payloads too large to be
-- a body: a document's body stays the indexed, queryable, size-capped part and
-- points at content by name.
--
-- Writes go to a pending version and become visible only at commit, so a
-- reader never sees a torn or half-written stream and an abandoned upload is
-- collectable by version rather than corrupting what is there.
CREATE TABLE content_meta (
    partition_key TEXT        NOT NULL,
    doc_id        TEXT        NOT NULL,
    name          TEXT        NOT NULL,
    -- The committed version readers see; NULL until the first commit.
    version       BIGINT,
    -- The version writes are currently going to; NULL when none is open.
    pending       BIGINT,
    size_bytes    BIGINT      NOT NULL DEFAULT 0,
    pending_bytes BIGINT      NOT NULL DEFAULT 0,
    content_type  TEXT,
    digest        TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (partition_key, doc_id, name)
);

-- One row per write. Chunks are never rewritten, so a write is an insert and
-- a range read selects the chunks that overlap it; `start_offset` and
-- `byte_len` are what make that a range query rather than a scan.
CREATE TABLE content_chunk (
    partition_key TEXT   NOT NULL,
    doc_id        TEXT   NOT NULL,
    name          TEXT   NOT NULL,
    version       BIGINT NOT NULL,
    start_offset  BIGINT NOT NULL,
    byte_len      BIGINT NOT NULL,
    bytes         BYTEA  NOT NULL,
    PRIMARY KEY (partition_key, doc_id, name, version, start_offset)
);

-- Reading a range, and dropping a superseded or abandoned version, are the
-- only two things that touch chunks.
CREATE INDEX content_chunk_range
    ON content_chunk (partition_key, doc_id, name, version, start_offset, byte_len);
