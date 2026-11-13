-- The boss's decision log (the boss understands sessions, H4): what it did
-- for a session, or chose not to do, each with why and how it is undone.
-- Recorded on the hub as it acts, never read out of what it said.
CREATE TABLE boss_decisions (
    id          UUID        PRIMARY KEY,
    -- The boss's agent id.
    boss        TEXT        NOT NULL,
    owner       TEXT        NOT NULL DEFAULT '',
    at          TIMESTAMPTZ NOT NULL,
    session     UUID,
    -- approval | question | choice | stall | turn_end | order
    need        TEXT        NOT NULL,
    -- The approval id, line message id or open item id it settled.
    need_ref    TEXT,
    what        TEXT        NOT NULL,
    why         TEXT        NOT NULL DEFAULT '',
    -- own | preference | escalated | person_first
    how         TEXT        NOT NULL,
    -- The preference it applied (no table yet: H9).
    preference  UUID,
    -- What its undo does, kept as it was recorded.
    action      JSONB       NOT NULL DEFAULT 'null',
    -- withdraw | reverse | correct | none
    undo        TEXT        NOT NULL DEFAULT 'none',
    undone_at   TIMESTAMPTZ,
    -- The Activity event it also wrote, when known.
    event       TEXT
);
CREATE INDEX boss_decisions_boss_at ON boss_decisions (boss, at DESC);
CREATE INDEX boss_decisions_session ON boss_decisions (session, at DESC) WHERE session IS NOT NULL;
