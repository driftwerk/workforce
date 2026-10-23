-- Per-call description for cost attribution UI. Records the high-level
-- intent of an LLM call ("vision:gpt-4o", "transcription:42s", a truncated
-- task prompt, etc.) so operators can answer "what was this $0.04 for?"
-- without joining back to session history. NULL on rows written before
-- this migration and on any path that doesn't supply a description.
ALTER TABLE cost_records   ADD COLUMN IF NOT EXISTS description TEXT;
ALTER TABLE task_telemetry ADD COLUMN IF NOT EXISTS description TEXT;
