-- When the person's answer on an item's buttons reached its boss: taken
-- into the turn running in its DM, or that DM's next turn ran with it. An
-- answer the hub stopped before it got there has none, and is opened again
-- at the next start (DW-763). Additive.
ALTER TABLE boss_open_items ADD COLUMN IF NOT EXISTS delivered_at TIMESTAMPTZ;
