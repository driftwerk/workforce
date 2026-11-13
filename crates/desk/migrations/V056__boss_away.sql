-- The person away from a boss's DM (overseer H8): since when the boss's
-- posts have gone unread past `away_after_minutes`. Updates gather while
-- it is set, and one catch-up goes out when they are back; a restart
-- keeps both.
ALTER TABLE boss_update_state ADD COLUMN away_since TIMESTAMPTZ;
