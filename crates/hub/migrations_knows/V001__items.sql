-- What a person's Desk shares with one of this hub's agents of their work
-- (Sees): one row per thing shared, kept by whose it is (the person, the
-- minting device and the agent) and its kind and ref, replaced when Desk
-- sends it again. Read only in the person's own turns with the agent.
CREATE TABLE items (
    person      UUID        NOT NULL,
    source      TEXT        NOT NULL,
    agent       TEXT        NOT NULL,
    -- account | decision | need | board | transcript
    kind        TEXT        NOT NULL,
    ref         TEXT        NOT NULL,
    session     TEXT,
    group_name  TEXT,
    at          TIMESTAMPTZ NOT NULL,
    body        JSONB       NOT NULL,
    bytes       INT         NOT NULL,
    received_at TIMESTAMPTZ NOT NULL,
    tsv         TSVECTOR GENERATED ALWAYS AS (to_tsvector('simple', coalesce(body->>'text', ''))) STORED,
    PRIMARY KEY (person, source, agent, kind, ref)
);
CREATE INDEX items_session ON items (person, agent, session, at);
CREATE INDEX items_kind_at ON items (kind, at);
CREATE INDEX items_tsv ON items USING GIN (tsv);
