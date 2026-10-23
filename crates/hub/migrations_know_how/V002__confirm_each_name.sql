-- Know-how is confirmed name by name: a table entry is shared once enough
-- different people read that table, a column once enough read that column,
-- a query shape once every name in it is shared. Entries learned under the
-- earlier rule (tables and columns together) are dropped; they were hours
-- old and in shadow.
TRUNCATE entries, provenance, intents, events RESTART IDENTITY CASCADE;

CREATE TABLE IF NOT EXISTS column_people (
    agent_id    TEXT NOT NULL,
    site        TEXT NOT NULL,
    database_id BIGINT NOT NULL,
    -- `table.column`
    name        TEXT NOT NULL,
    person      TEXT NOT NULL,
    last_seen   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (agent_id, site, database_id, name, person)
);
