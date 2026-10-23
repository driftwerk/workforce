-- One row per inbound Slack message that a hub task has claimed the right to
-- answer. The primary key makes `INSERT … ON CONFLICT DO NOTHING` an atomic
-- cross-task claim; `claimed_at` drives time-based pruning.

CREATE TABLE IF NOT EXISTS response_claims (
    agent_id     TEXT        NOT NULL,
    workspace_id TEXT        NOT NULL,
    channel      TEXT        NOT NULL,
    ts           TEXT        NOT NULL,
    claimed_by   TEXT        NOT NULL,
    claimed_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (agent_id, workspace_id, channel, ts)
);

CREATE INDEX IF NOT EXISTS response_claims_claimed_at_idx ON response_claims (claimed_at);
