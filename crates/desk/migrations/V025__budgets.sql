-- Budgets the person sets, and where each subject's breaker stands. Both
-- start empty: nothing is limited until the person sets a budget.
CREATE TABLE budgets (
    id         UUID PRIMARY KEY,
    -- desk | each_session | each_agent | session:<uuid> | agent:<id>
    scope      TEXT NOT NULL,
    -- The sidebar group it is the budget of; null is ungrouped. An item's
    -- own budget (session:, agent:) is keyed by the item and has none, so
    -- it goes with the item when it moves.
    group_id   TEXT,
    period     TEXT NOT NULL,
    unit       TEXT NOT NULL,
    amount     DOUBLE PRECISION NOT NULL CHECK (amount > 0),
    warn_at    DOUBLE PRECISION NOT NULL DEFAULT 0.8 CHECK (warn_at > 0 AND warn_at < 1),
    on_exceed  TEXT NOT NULL DEFAULT 'pause',
    updated_at TIMESTAMPTZ NOT NULL
);
CREATE UNIQUE INDEX budgets_one ON budgets (COALESCE(group_id, ''), scope, period, unit);
-- Where each subject's breaker stands, so a restart neither forgets a hold
-- nor repeats a warning.
CREATE TABLE breakers (
    -- session:<uuid> | agent:<id> | pool:desk | pool:group:<id>
    subject    TEXT PRIMARY KEY,
    state      JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
