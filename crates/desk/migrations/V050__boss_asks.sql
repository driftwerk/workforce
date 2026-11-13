-- What a session asks the person, sent up a boss link so they can answer
-- it from anywhere (remote bosses, RB7b). Kept on the engine whose session
-- asked: the only place that decides the answer. The nonce an answer must
-- bring is kept as its hash, and is good once.
CREATE TABLE boss_asks (
    id          UUID        PRIMARY KEY,
    link_id     UUID        NOT NULL,
    group_id    TEXT        NOT NULL,
    session     UUID        NOT NULL,
    -- The request it asks about.
    approval    UUID        NOT NULL,
    nonce_hash  TEXT        NOT NULL,
    -- anywhere | desk_only
    class       TEXT        NOT NULL,
    -- The answers it offers from elsewhere: allow, deny, stop.
    options     JSONB       NOT NULL,
    -- open | decided | answered_in_desk | answered_by_boss | expired | withdrawn
    state       TEXT        NOT NULL DEFAULT 'open',
    choice      TEXT,
    -- Who answered, as the boss hub said.
    decider     JSONB,
    created_at  TIMESTAMPTZ NOT NULL,
    expires_at  TIMESTAMPTZ NOT NULL,
    closed_at   TIMESTAMPTZ,
    UNIQUE (link_id, approval)
);
CREATE INDEX boss_asks_open ON boss_asks (link_id) WHERE state = 'open';
