-- Which boss runs which session: one row per session. A release keeps the
-- row, so a boss that takes on every session never takes it again.
-- `wheel` is who drives it now: the boss, or the person (`person_pinned`
-- until they hand it back). Sessions are written behind, so a charge can
-- be made before its session's row is there: forgetting a session deletes
-- its charge, as it does the rest of what it leaves.
CREATE TABLE charges (
    session     UUID        PRIMARY KEY,
    -- The boss's sidebar group; null is the main boss.
    group_id    TEXT,
    -- The overseer's agent id.
    boss        TEXT        NOT NULL,
    how         TEXT        NOT NULL,   -- 'started' | 'handed' | 'taken'
    wheel       TEXT        NOT NULL DEFAULT 'boss',
    wheel_since TIMESTAMPTZ,
    since       TIMESTAMPTZ NOT NULL,
    released_at TIMESTAMPTZ,
    updated_at  TIMESTAMPTZ NOT NULL
);
CREATE INDEX charges_boss ON charges (boss) WHERE released_at IS NULL;
