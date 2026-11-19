-- The waits a boss starts itself (`crew.watch`): a pull request, a CI run
-- or a time, polled by the hub as the person until it ends, and its end
-- told to the boss once. Additive.
CREATE TABLE IF NOT EXISTS boss_watches (
    id UUID PRIMARY KEY,
    boss TEXT NOT NULL,
    person TEXT NOT NULL,
    kind TEXT NOT NULL,
    repo TEXT,
    number BIGINT,
    run_id BIGINT,
    until_merged BOOLEAN NOT NULL DEFAULT FALSE,
    note TEXT NOT NULL DEFAULT '',
    session UUID,
    plan UUID,
    created_at TIMESTAMPTZ NOT NULL,
    due_at TIMESTAMPTZ,
    expires_at TIMESTAMPTZ NOT NULL,
    polled_at TIMESTAMPTZ,
    failures INTEGER NOT NULL DEFAULT 0,
    ended_at TIMESTAMPTZ,
    outcome TEXT,
    delivered_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS boss_watches_open
    ON boss_watches (boss, person) WHERE ended_at IS NULL;
CREATE INDEX IF NOT EXISTS boss_watches_undelivered
    ON boss_watches (boss, person) WHERE ended_at IS NOT NULL AND delivered_at IS NULL;
