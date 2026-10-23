-- The engine is a fact about a database and belongs beside its names, so a
-- replica that never listed databases still publishes the dialect hint.
ALTER TABLE databases ADD COLUMN IF NOT EXISTS engine TEXT;
