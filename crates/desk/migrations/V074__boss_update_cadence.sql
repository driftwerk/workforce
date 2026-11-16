-- Boss updates as a teammate (designs/desk-updates-teammate.md): which of
-- a boss's decisions are important enough to be told ("Decided without
-- you"), judged when each is made; and the update state's two cursors for
-- the cadence: what has been routed (said right away, or left for the
-- roundup), and when the last roundup went. Additive.
ALTER TABLE boss_decisions ADD COLUMN important TEXT;
ALTER TABLE boss_update_state ADD COLUMN routed_at TIMESTAMPTZ;
ALTER TABLE boss_update_state ADD COLUMN roundup_at TIMESTAMPTZ;
-- A boss already running starts its roundups from its last update, never
-- from when it began.
UPDATE boss_update_state SET roundup_at = COALESCE(last_update_at, since);
