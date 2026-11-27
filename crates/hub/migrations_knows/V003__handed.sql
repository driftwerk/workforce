-- A person's request an agent here handed to their Desk boss (DW-770):
-- their words, the session it is about and where they asked, taken by the
-- Desk on their own computer, and Desk's words on it told back there.
CREATE TABLE handed (
    id           UUID        PRIMARY KEY,
    person       UUID        NOT NULL,
    agent        TEXT        NOT NULL,
    agent_name   TEXT        NOT NULL,
    words        TEXT        NOT NULL,
    session      TEXT,
    session_name TEXT,
    platform     TEXT        NOT NULL,
    channel      TEXT        NOT NULL,
    thread       TEXT,
    asked_at     TIMESTAMPTZ NOT NULL,
    taken_at     TIMESTAMPTZ,
    taken_by     TEXT,
    heard_at     TIMESTAMPTZ,
    said         INT         NOT NULL DEFAULT 0,
    ended_at     TIMESTAMPTZ,
    ended        TEXT
);
CREATE INDEX handed_waiting ON handed (person, agent) WHERE ended_at IS NULL AND heard_at IS NULL;
-- When each of a person's Desks last checked in for requests.
CREATE TABLE checked_in (
    person UUID        NOT NULL,
    source TEXT        NOT NULL,
    agent  TEXT        NOT NULL,
    at     TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (person, source, agent)
);
