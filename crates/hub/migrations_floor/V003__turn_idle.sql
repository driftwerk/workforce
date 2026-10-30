-- Where a turn waited on its own background work.
ALTER TABLE floor.turns ADD COLUMN idle JSONB NOT NULL DEFAULT '[]';
