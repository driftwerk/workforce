CREATE SCHEMA IF NOT EXISTS floor;

-- One row per turn of work: an agent's, or one of Desk's sessions'.
CREATE TABLE floor.turns (
    id          UUID PRIMARY KEY,
    owner       TEXT,
    subject     TEXT NOT NULL,
    subject_doc JSONB NOT NULL,
    name        TEXT NOT NULL,
    place       JSONB NOT NULL,
    task        TEXT,
    doing       TEXT,
    started_at  TIMESTAMPTZ NOT NULL,
    heard_at    TIMESTAMPTZ NOT NULL,
    ended_at    TIMESTAMPTZ,
    outcome     TEXT,
    answer      TEXT,
    question    JSONB,
    settled_at  TIMESTAMPTZ,
    waits       JSONB NOT NULL DEFAULT '[]',
    token       TEXT,
    instance    TEXT
);

CREATE INDEX turns_owner_started ON floor.turns (owner, started_at DESC);
CREATE INDEX turns_open ON floor.turns (owner) WHERE ended_at IS NULL;
CREATE INDEX turns_asking ON floor.turns (owner, subject) WHERE outcome = 'asked' AND settled_at IS NULL;
