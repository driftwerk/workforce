-- What a goal's boss was told of it already by the hub's goal checks
-- (DW-729): each part's stall and its spell, each run of failed reads, the
-- messages it was told hadn't gone in, and the note last given the
-- person's roundup. Kept with the plan, so a restart says none again.
-- Additive.
ALTER TABLE plans ADD COLUMN IF NOT EXISTS goal_said JSONB NOT NULL DEFAULT '{}'::jsonb;
