-- The audit of what was asked of this engine about bosses (remote bosses,
-- RB1): Desk's writes of grants, stops and pauses now, and each boss's crew
-- call once bosses are served. Kept on the engine that did it, the only
-- place that knows what was done.
CREATE TABLE boss_calls (
    id         UUID        PRIMARY KEY,
    at         TIMESTAMPTZ NOT NULL,
    -- The boss link it came on; null for what isn't a boss's call.
    link_id    TEXT,
    -- The boss hub's id for the call: one call is acted on once.
    call_id    TEXT,
    -- desk | machine | boss:<hub>/<agent>
    caller     TEXT        NOT NULL,
    boss       TEXT,
    group_id   TEXT,
    tool       TEXT        NOT NULL,
    -- What was asked, redacted, and the hash of all of it.
    input      TEXT        NOT NULL,
    input_hash TEXT        NOT NULL,
    -- ok | refused | suggested | escalated
    outcome    TEXT        NOT NULL,
    said       TEXT,
    -- What went back: sizes and redaction counts, never the content.
    result     JSONB       NOT NULL DEFAULT '{}',
    decision   JSONB,
    UNIQUE (link_id, call_id)
);
CREATE INDEX boss_calls_at ON boss_calls (at);
CREATE INDEX boss_calls_group ON boss_calls (group_id, at);
