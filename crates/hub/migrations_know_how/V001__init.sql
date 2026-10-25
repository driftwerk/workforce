-- Agent know-how (docs/agent-know-how-design.md). Entries hold only names
-- checked against something real; people are stored as per-agent hashes.

CREATE TABLE IF NOT EXISTS entries (
    id          BIGSERIAL PRIMARY KEY,
    agent_id    TEXT NOT NULL,
    kind        TEXT NOT NULL,
    key         TEXT NOT NULL,
    site        TEXT NOT NULL,
    database_id BIGINT NOT NULL,
    tables      TEXT[] NOT NULL DEFAULT '{}',
    columns     TEXT[] NOT NULL DEFAULT '{}',
    shape       TEXT,
    note        TEXT,
    -- Added by hand: shown to others only once an admin pins it, and never
    -- confirmed by turns.
    manual      BOOLEAN NOT NULL DEFAULT FALSE,
    pinned      BOOLEAN NOT NULL DEFAULT FALSE,
    -- Removed by an admin; stays removed.
    retired     BOOLEAN NOT NULL DEFAULT FALSE,
    people      INTEGER NOT NULL DEFAULT 0,
    uses        INTEGER NOT NULL DEFAULT 0,
    first_seen  TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (agent_id, key)
);

CREATE TABLE IF NOT EXISTS provenance (
    entry_id    BIGINT NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    person      TEXT NOT NULL,
    first_seen  TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (entry_id, person)
);

-- What a request that led to an entry was after, abstracted (no names, no
-- values), as its embedding only: the text is not kept. Matching keys, never
-- shown. One per person per entry.
CREATE TABLE IF NOT EXISTS intents (
    id          BIGSERIAL PRIMARY KEY,
    entry_id    BIGINT NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    person      TEXT NOT NULL,
    model       TEXT NOT NULL,
    vector      REAL[] NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS intents_entry ON intents (entry_id);

CREATE TABLE IF NOT EXISTS events (
    id          BIGSERIAL PRIMARY KEY,
    agent_id    TEXT NOT NULL,
    entry_id    BIGINT,
    kind        TEXT NOT NULL,
    score       REAL,
    at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS events_agent_at ON events (agent_id, at);
