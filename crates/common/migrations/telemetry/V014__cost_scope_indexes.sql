-- Budget-enforcement scope lookups. The cost governor answers "spend today
-- for this identity/agent" on the turn admission path, and the scheduler
-- totals one job's spend by task_id prefix before each fire — all three need
-- indexes (V001/V003 shipped only single-column identity/agent/timestamp).
CREATE INDEX IF NOT EXISTS idx_cost_identity_ts ON cost_records(identity_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_cost_agent_ts ON cost_records(agent_id, timestamp);
-- COLLATE "C" so the range-form prefix scan in cost_by_task_prefix is
-- index-served regardless of the database's default collation.
CREATE INDEX IF NOT EXISTS idx_cost_task ON cost_records(task_id COLLATE "C");
