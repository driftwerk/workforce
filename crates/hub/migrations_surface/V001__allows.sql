CREATE SCHEMA IF NOT EXISTS surface;
CREATE TABLE IF NOT EXISTS surface.allows (
    id          UUID        PRIMARY KEY,
    person      TEXT        NOT NULL,
    agent_id    TEXT        NOT NULL,
    platform    TEXT        NOT NULL,
    destination TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL,
    UNIQUE (person, agent_id, platform, destination)
);
