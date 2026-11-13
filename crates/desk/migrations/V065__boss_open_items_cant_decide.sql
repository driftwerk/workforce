-- Why the boss couldn't decide an ask itself (overseer, the brief update):
-- at Act fully an ask carries answers only with a reason here; without
-- one the boss decides and acts, and the person is told what it did.
-- Additive: older rows have none.
ALTER TABLE boss_open_items ADD COLUMN cant_decide TEXT;
