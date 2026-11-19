-- The open orders a boss gives its sessions (designs/boss-orders-and-serial-turns.md
-- §3): who holds which job, so a second session is never handed the same
-- one and a session is never told twice to start what it already started.
-- Each row is reserved (`sending`) under the boss's advisory lock before
-- its line message goes, then `open` or `failed`. Additive.
CREATE TABLE IF NOT EXISTS boss_assignments (
    id UUID PRIMARY KEY,
    boss TEXT NOT NULL,
    session UUID NOT NULL,
    machine TEXT,
    line_message UUID,
    text TEXT NOT NULL,
    job TEXT,
    job_key TEXT,
    repo TEXT,
    targets TEXT[] NOT NULL DEFAULT '{}',
    irreversible BOOLEAN NOT NULL DEFAULT FALSE,
    urgent BOOLEAN NOT NULL DEFAULT FALSE,
    -- Whether it holds its job against other sessions: irreversible, or
    -- a job the boss named.
    claims BOOLEAN NOT NULL DEFAULT FALSE,
    state TEXT NOT NULL,
    sent_at TIMESTAMPTZ NOT NULL,
    delivered_at TIMESTAMPTZ,
    acked_at TIMESTAMPTZ,
    closed_at TIMESTAMPTZ,
    closed_by TEXT,
    closed_why TEXT,
    replaces UUID,
    turn TEXT,
    item UUID,
    -- While held: the stand-down its old holder is to answer, the answer
    -- when it comes, the action to send then, and the picture's number
    -- it was decided on.
    stand_down UUID,
    answer TEXT,
    action JSONB,
    basis BIGINT
);
CREATE INDEX IF NOT EXISTS boss_assignments_live
    ON boss_assignments (boss, state);
CREATE INDEX IF NOT EXISTS boss_assignments_line
    ON boss_assignments (line_message);
-- Two live claims on one job, whatever reached the table: refused by
-- Postgres too.
CREATE UNIQUE INDEX IF NOT EXISTS boss_assignments_one_holder
    ON boss_assignments (boss, job_key)
    WHERE claims AND job_key IS NOT NULL AND state IN ('sending', 'open', 'held');
