-- Session memory: what each session said and did, harvested from its
-- engine's own record and redacted before it is kept.

-- One prompt and what the session did for it, redacted, as its engine
-- recorded it. The first session to read an exchange's key keeps it.
CREATE TABLE memory_exchanges (
    id         UUID        PRIMARY KEY,
    engine     TEXT        NOT NULL,
    key        TEXT        NOT NULL,
    session    UUID        NOT NULL,
    group_id   TEXT,
    project    TEXT        NOT NULL,
    folder     TEXT        NOT NULL,
    seq        INT         NOT NULL,
    at         TIMESTAMPTZ NOT NULL,
    ended      TIMESTAMPTZ,               -- NULL: still being written
    asker      TEXT        NOT NULL,
    ask        TEXT        NOT NULL,
    answer     TEXT        NOT NULL,
    steps      JSONB       NOT NULL,
    files      TEXT[]      NOT NULL DEFAULT '{}',
    commands   TEXT[]      NOT NULL DEFAULT '{}',
    errors     TEXT[]      NOT NULL DEFAULT '{}',
    compaction BOOLEAN     NOT NULL DEFAULT false,
    redacted   INT         NOT NULL DEFAULT 0,
    flagged    TEXT[]      NOT NULL DEFAULT '{}',
    tsv        TSVECTOR    GENERATED ALWAYS AS (to_tsvector('simple', ask || ' ' || answer)) STORED,
    indexed_at TIMESTAMPTZ,               -- NULL: not yet in the search index (the outbox)
    UNIQUE (engine, key)
);
CREATE INDEX memory_exchanges_session ON memory_exchanges (session, seq);
CREATE INDEX memory_exchanges_scope ON memory_exchanges (group_id, project, at);
CREATE INDEX memory_exchanges_tsv ON memory_exchanges USING GIN (tsv);
CREATE INDEX memory_exchanges_outbox ON memory_exchanges (at) WHERE indexed_at IS NULL AND ended IS NOT NULL;

-- Whose each session's memory is, as it was when last harvested: its name,
-- engine and records, so what it remembered can still be named, read and
-- forgotten once the session itself is cleared away.
CREATE TABLE memory_sessions (
    session        UUID        PRIMARY KEY,
    engine         TEXT        NOT NULL,
    name           TEXT        NOT NULL,
    cli_session_id TEXT,
    sources        TEXT[]      NOT NULL DEFAULT '{}',
    updated_at     TIMESTAMPTZ NOT NULL
);

-- What Desk noted from a session: decisions, facts and todos, each citing
-- its exchanges.
CREATE TABLE memory_items (
    id            UUID        PRIMARY KEY,
    session       UUID        NOT NULL,
    group_id      TEXT,
    project       TEXT        NOT NULL,
    kind          TEXT        NOT NULL,   -- 'decision' | 'fact' | 'todo'
    text          TEXT        NOT NULL,
    state         TEXT        NOT NULL,   -- todo: 'open' | 'done' | 'dropped'; others: 'held'
    valid_from    TIMESTAMPTZ NOT NULL,
    valid_to      TIMESTAMPTZ,
    superseded_by UUID        REFERENCES memory_items (id) ON DELETE SET NULL,
    sources       UUID[]      NOT NULL CHECK (cardinality(sources) > 0),
    by            TEXT        NOT NULL,   -- 'model:<name>' | 'person'
    confirmed     BOOLEAN     NOT NULL DEFAULT false,
    written_at    TIMESTAMPTZ NOT NULL,
    indexed_at    TIMESTAMPTZ
);
CREATE INDEX memory_items_session ON memory_items (session, kind) WHERE valid_to IS NULL;

-- The durable queue for derivation: how far each session's exchanges have
-- been read for items.
CREATE TABLE memory_derive (
    session    UUID PRIMARY KEY,
    derived_to INT  NOT NULL DEFAULT 0,
    due_at     TIMESTAMPTZ,
    tries      INT  NOT NULL DEFAULT 0,
    failed     TEXT
);

-- Who read what, for the panel and audit.
CREATE TABLE memory_reads (
    id       UUID        PRIMARY KEY,
    at       TIMESTAMPTZ NOT NULL,
    reader   TEXT        NOT NULL,        -- 'person' | 'overseer:<scope>' | 'session:<id>'
    tool     TEXT        NOT NULL,
    query    TEXT,
    hits     UUID[]      NOT NULL,
    group_id TEXT
);
CREATE INDEX memory_reads_at ON memory_reads (at);

-- Records never to read again.
CREATE TABLE memory_forgotten (
    engine TEXT        NOT NULL,
    ref    TEXT        NOT NULL,
    at     TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (engine, ref)
);

-- Index deletions not yet done.
CREATE TABLE memory_purges (
    id         UUID        PRIMARY KEY,
    collection TEXT        NOT NULL,
    filter     JSONB       NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    done_at    TIMESTAMPTZ,
    tries      INT         NOT NULL DEFAULT 0
);

-- How far each record's conversation has been harvested, beside how far
-- the ledger has read it, and what the harvest carries between reads.
ALTER TABLE ledger_cursors ADD COLUMN harvested_to BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN harvest JSONB NOT NULL DEFAULT '{}';

-- The person's switch per session: 'on' | 'off'.
ALTER TABLE sessions ADD COLUMN memory TEXT NOT NULL DEFAULT 'on';
