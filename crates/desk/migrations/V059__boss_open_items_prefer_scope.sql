-- The scope an "always" answer to one of the boss's asks makes its
-- preference for (overseer H10): all, group:<id> or repo:<owner/name>.
-- Additive.
ALTER TABLE boss_open_items ADD COLUMN prefer_scope TEXT;
-- One kept preference per ask's "always", however often it is pressed.
CREATE UNIQUE INDEX boss_preferences_from_item ON boss_preferences (from_item)
    WHERE from_item IS NOT NULL AND state <> 'retired';
