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
