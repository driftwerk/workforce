-- What each session spent, as its engine recorded it: one row per model
-- call, kept once by the call's own key. A forked or resumed conversation,
-- and a sub-agent forked from one, copy their history's calls under the
-- same keys, so the key is unique per engine and the session that had a
-- call first keeps it.
CREATE TABLE ledger (
    engine      TEXT        NOT NULL,
    key         TEXT        NOT NULL,
    session     UUID        NOT NULL,
    at          TIMESTAMPTZ NOT NULL,
    model       TEXT,
    input       BIGINT      NOT NULL DEFAULT 0,
    output      BIGINT      NOT NULL DEFAULT 0,
    cache_read  BIGINT      NOT NULL DEFAULT 0,
    cache_write BIGINT      NOT NULL DEFAULT 0,
    reasoning   BIGINT      NOT NULL DEFAULT 0,
    -- At API list prices; null where the hub's table has no price.
    usd         DOUBLE PRECISION,
    priced      TEXT        NOT NULL,   -- 'priced' | 'reported' | 'none'
    billing     TEXT        NOT NULL,   -- 'plan' | 'key' | 'vendor' | 'unknown'
    -- The CLI's own account name, where Desk knew who was signed in then.
    account     TEXT,
    PRIMARY KEY (engine, key)
);
CREATE INDEX ledger_at ON ledger (at);
CREATE INDEX ledger_session_at ON ledger (session, at);
-- How far each of a session's records (a transcript, each sub-agent's, a
-- rollout) has been read, and what its reader carries from line to line.
CREATE TABLE ledger_cursors (
    session    UUID        NOT NULL,
    source     TEXT        NOT NULL,
    read_to    BIGINT      NOT NULL,
    seen       JSONB       NOT NULL DEFAULT '{}',
    updated_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (session, source)
);
