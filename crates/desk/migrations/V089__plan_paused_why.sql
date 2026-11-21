-- Why a plan paused for a ticket that keeps failing to start, in the
-- person's words, so the Floor and its card say it again after a restart.
-- Additive: null for every plan there is.
ALTER TABLE plans ADD COLUMN paused_why TEXT;
