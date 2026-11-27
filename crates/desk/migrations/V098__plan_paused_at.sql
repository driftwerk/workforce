-- When a plan's pause began, and the boss it was paused under.
ALTER TABLE plans ADD COLUMN IF NOT EXISTS paused_at TIMESTAMPTZ;
ALTER TABLE plans ADD COLUMN IF NOT EXISTS paused_for TEXT;
