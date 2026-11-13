-- What each agent knows of the work in Desk ("What they see", the Sees
-- design): one rule per agent, kept here and never on the agent's own hub,
-- what was shared with it under the rule, how far each kind has been
-- shared, and the person's changes and choices about what it holds.
-- Additive.
CREATE TABLE knows_rules (
    -- The agent's member key: local/<agent> or <hub>/<agent>.
    agent       TEXT        PRIMARY KEY,
    -- all | group:<id> | board:<id> | grid:<id> | session:<id> |
    -- session:<machine>/<id> | agent:<hub>/<agent>. Empty: its own work.
    places      TEXT[]      NOT NULL DEFAULT '{}',
    transcripts BOOLEAN     NOT NULL DEFAULT false,
    -- off | live | daily
    digest      TEXT        NOT NULL DEFAULT 'off',
    revision    BIGINT      NOT NULL DEFAULT 1,
    changed_at  TIMESTAMPTZ NOT NULL
);

-- Each thing shared with an agent: the audit of what went where, and what
-- the agent holds, for a narrowing to count and delete.
CREATE TABLE knows_sent (
    agent      TEXT        NOT NULL,
    -- account | decision | need | board | transcript
    kind       TEXT        NOT NULL,
    ref        TEXT        NOT NULL,
    session    UUID,
    group_id   TEXT,
    board      UUID,
    -- The boss a decision is of, as charges name it.
    boss       TEXT,
    bytes      INT         NOT NULL,
    sent_at    TIMESTAMPTZ NOT NULL,
    -- The person chose to let the agent keep it, outside its rule.
    kept_at    TIMESTAMPTZ,
    -- The delete (a knows_log row) the person chose for it, until done.
    deleting   UUID,
    -- The agent's copy was deleted.
    deleted_at TIMESTAMPTZ,
    PRIMARY KEY (agent, kind, ref)
);
CREATE INDEX knows_sent_held ON knows_sent (agent, session) WHERE deleted_at IS NULL;
CREATE INDEX knows_sent_at ON knows_sent (agent, sent_at);

-- How far each kind has been shared with each agent.
CREATE TABLE knows_cursors (
    agent  TEXT  NOT NULL,
    kind   TEXT  NOT NULL,
    cursor JSONB NOT NULL DEFAULT '{}',
    PRIMARY KEY (agent, kind)
);

-- The rule's changes, and each delete and keep of what an agent held: counts
-- and places, never content. A delete waits (`done_at` NULL) until the
-- agent's hub has done it.
CREATE TABLE knows_log (
    id      UUID        PRIMARY KEY,
    at      TIMESTAMPTZ NOT NULL,
    agent   TEXT        NOT NULL,
    -- rule | delete | keep
    what    TEXT        NOT NULL,
    detail  JSONB       NOT NULL DEFAULT '{}',
    done_at TIMESTAMPTZ
);
CREATE INDEX knows_log_agent ON knows_log (agent, at DESC);
CREATE INDEX knows_log_pending ON knows_log (at) WHERE what = 'delete' AND done_at IS NULL;
