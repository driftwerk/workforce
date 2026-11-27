-- Until when the person's plan limit pauses a plan.
ALTER TABLE plans ADD COLUMN IF NOT EXISTS paused_until TIMESTAMPTZ;
