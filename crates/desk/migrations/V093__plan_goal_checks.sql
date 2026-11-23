-- What the hub's goal checks keep with a plan (DW-729). `goal_said`: what
-- its boss was told already (each part's stall and its spell, each run of
-- failed reads, the messages it was told hadn't gone in, and the note last
-- given the person's roundup), so a restart says none again. `goal_heads`:
-- where the goal's branches and pull requests stood when it was agreed or
-- a checkpoint last moved it, which a commit shown toward it must come
-- after. Additive.
ALTER TABLE plans ADD COLUMN IF NOT EXISTS goal_said JSONB NOT NULL DEFAULT '{}'::jsonb;
ALTER TABLE plans ADD COLUMN IF NOT EXISTS goal_heads JSONB NOT NULL DEFAULT '{}'::jsonb;
