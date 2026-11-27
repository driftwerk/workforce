-- Until when a plan is paused by the person's plan limit (DW-760): the
-- window holding its boss, so a restart keeps it paused until then.
-- Additive: null for every plan there is.
ALTER TABLE plans ADD COLUMN IF NOT EXISTS paused_until TIMESTAMPTZ;
