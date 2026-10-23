-- One row per scheduled job, scoped per agent. The whole ScheduledJob lives
-- in `definition` so the schema survives job-shape evolution; `created_at`
-- is promoted for a stable listing order.
CREATE TABLE jobs (
    agent_id TEXT NOT NULL,
    id TEXT NOT NULL,
    definition JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (agent_id, id)
);
