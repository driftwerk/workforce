-- The scope an "always" answer to one of the boss's asks makes its
-- preference for (overseer H10): all, group:<id> or repo:<owner/name>.
-- Additive.
ALTER TABLE boss_open_items ADD COLUMN prefer_scope TEXT;
