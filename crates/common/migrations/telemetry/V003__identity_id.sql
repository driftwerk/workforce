-- Per-identity stats dimension. Captures which user (canonical identity
-- UUID) triggered each task and cost record so the UI can drill into
-- per-identity usage. NULL for rows written before this migration and
-- for system-initiated tasks (workflow runtime, knowledge maintenance,
-- internal jobs without an identity in scope).
ALTER TABLE cost_records   ADD COLUMN IF NOT EXISTS identity_id TEXT;
ALTER TABLE task_telemetry ADD COLUMN IF NOT EXISTS identity_id TEXT;
CREATE INDEX IF NOT EXISTS idx_cost_records_identity   ON cost_records(identity_id);
CREATE INDEX IF NOT EXISTS idx_task_telemetry_identity ON task_telemetry(identity_id);
