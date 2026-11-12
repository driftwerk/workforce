-- Each session's running account (the boss understands sessions, H1): what
-- it is for, what it did, what it is doing and what comes next, kept
-- current from its own conversation by the hub's cheap model, and read by
-- every turn of the boss.
CREATE TABLE session_accounts (
    session     UUID        PRIMARY KEY,
    owner       TEXT        NOT NULL DEFAULT '',
    purpose     TEXT        NOT NULL DEFAULT '',
    asked_by    TEXT        NOT NULL DEFAULT '',
    plan        TEXT        NOT NULL DEFAULT '',
    -- [{text, refs: [{kind, id, url?}], at}]
    done        JSONB       NOT NULL DEFAULT '[]',
    now         TEXT        NOT NULL DEFAULT '',
    waiting_on  TEXT,
    next        TEXT        NOT NULL DEFAULT '',
    -- [{what, why, at}]: choices the session itself made
    decisions   JSONB       NOT NULL DEFAULT '[]',
    -- The exchange keys already read: the newest's time, and the keys read
    -- within the hour before it.
    cursor      JSONB       NOT NULL DEFAULT '{}',
    -- Compare-and-set: a write lands only on the version it read.
    version     BIGINT      NOT NULL DEFAULT 1,
    written_at  TIMESTAMPTZ NOT NULL,
    by_model    BOOLEAN     NOT NULL DEFAULT false,
    stale_after TIMESTAMPTZ
);

-- Which exchanges each session's record holds, by key: an exchange is kept
-- once, by the session that read it first, so a resumed or forked session's
-- copied history is found through here, whichever session harvested it.
CREATE TABLE memory_record_keys (
    session UUID        NOT NULL,
    engine  TEXT        NOT NULL,
    key     TEXT        NOT NULL,
    at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (session, engine, key)
);
CREATE INDEX memory_record_keys_at ON memory_record_keys (session, at);

-- The cheap model's calls each day, in the person's time, by what they
-- were for ('account', 'question'), against `[oversight.updates]`
-- `cheap_calls_per_day` and `account_questions_per_day`.
CREATE TABLE desk_cheap_calls (
    day   DATE NOT NULL,
    kind  TEXT NOT NULL,
    calls INT  NOT NULL DEFAULT 0,
    PRIMARY KEY (day, kind)
);
