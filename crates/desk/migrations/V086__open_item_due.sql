-- When a promise of the boss's is chased next, if still owed (`due_at`),
-- and when its boss was told to go and get it itself (`chased_at`).
-- Additive.
ALTER TABLE boss_open_items ADD COLUMN IF NOT EXISTS due_at TIMESTAMPTZ;
ALTER TABLE boss_open_items ADD COLUMN IF NOT EXISTS chased_at TIMESTAMPTZ;
CREATE INDEX IF NOT EXISTS boss_open_items_due ON boss_open_items (due_at)
    WHERE due_at IS NOT NULL AND state <> 'closed';
