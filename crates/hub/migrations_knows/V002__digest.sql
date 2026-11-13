-- How each person's Desk wants an agent to hear of their work in their own
-- conversations (Sees K4): off | live | daily, as the rule there says.
CREATE TABLE owners (
    person     UUID        NOT NULL,
    source     TEXT        NOT NULL,
    agent      TEXT        NOT NULL,
    digest     TEXT        NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (person, source, agent)
);
CREATE INDEX items_reader ON items (person, agent, kind, at);
-- The daily digest as built for its window, kept until the next one so it
-- reads the same all day (an empty block: nothing had changed).
CREATE TABLE daily (
    person    UUID        NOT NULL,
    agent     TEXT        NOT NULL,
    built_for TIMESTAMPTZ NOT NULL,
    block     TEXT        NOT NULL,
    PRIMARY KEY (person, agent)
);
