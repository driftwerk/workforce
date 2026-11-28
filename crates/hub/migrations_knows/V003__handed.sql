-- Requests an agent here handed to the person's Desk boss (DW-770), and
-- when each of their Desks last checked in for them.
CREATE TABLE handed (
    id           UUID        PRIMARY KEY,
    person       UUID        NOT NULL,
    agent        TEXT        NOT NULL,
    agent_name   TEXT        NOT NULL,
    words        TEXT        NOT NULL,
    session      TEXT,
    session_name TEXT,
    platform     TEXT        NOT NULL,
    workspace    TEXT,
    channel      TEXT        NOT NULL,
    thread       TEXT,
    conversation TEXT,
    asked_at     TIMESTAMPTZ NOT NULL,
    taken_at     TIMESTAMPTZ,
    taken_by     TEXT,
    heard_at     TIMESTAMPTZ,
    said         INT         NOT NULL DEFAULT 0,
    ended_at     TIMESTAMPTZ,
    ended        TEXT
);
CREATE INDEX handed_waiting ON handed (person, agent) WHERE ended_at IS NULL AND heard_at IS NULL;
CREATE UNIQUE INDEX handed_open ON handed (person, agent, md5(words), coalesce(session, ''))
    WHERE ended_at IS NULL AND heard_at IS NULL;
CREATE TABLE checked_in (
    person UUID        NOT NULL,
    source TEXT        NOT NULL,
    agent  TEXT        NOT NULL,
    at     TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (person, source, agent)
);
