-- The sessions a promise or an order is linked to (the boss understands
-- sessions, H6): an order is the person's words plus the sessions working
-- on it, and it lapses once all of them have ended. Additive.
ALTER TABLE boss_open_items ADD COLUMN sessions UUID[] NOT NULL DEFAULT '{}';
