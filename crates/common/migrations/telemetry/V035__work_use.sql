-- What background work (a scheduled job, a monitor, a boss's own turns)
-- used: a row for each turn it began, the tokens of each call it made, and
-- the share of a plan's window each call moved. The plan guard reads the
-- last hour and week of a unit of work to tell a runaway from its usual;
-- Desk shows each one's day and week. Additive: a new table only.
CREATE TABLE IF NOT EXISTS work_use (
    id BIGSERIAL PRIMARY KEY,
    at TIMESTAMPTZ NOT NULL,
    work TEXT NOT NULL,
    label TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    payer TEXT,
    plan TEXT,
    window_minutes INTEGER,
    share DOUBLE PRECISION NOT NULL DEFAULT 0,
    turns INTEGER NOT NULL DEFAULT 0,
    tokens BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_work_use_at ON work_use (at);
CREATE INDEX IF NOT EXISTS idx_work_use_work_at ON work_use (work, at);
