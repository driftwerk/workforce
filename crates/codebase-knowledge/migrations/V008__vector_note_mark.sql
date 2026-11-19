-- A pending note's `seq` is the mark it was made under: the counter's
-- `last` as one call to note files took it, one above the call before's.
-- A call holds the counter's row until it commits, so marks are taken in
-- the order their notes are committed: a consistent copy of the graph
-- holds every note at or below its `last`. Seeded at the highest `seq`
-- noted already, so that every note made before is at or below every mark
-- taken after.
CREATE TABLE IF NOT EXISTS vector_note_mark (
    only_one INTEGER PRIMARY KEY CHECK (only_one = 1),
    last     BIGINT NOT NULL
);

INSERT INTO vector_note_mark (only_one, last)
SELECT 1, COALESCE(max(seq), 0) FROM vector_pending
ON CONFLICT (only_one) DO NOTHING;

-- The mark as each lease was taken: every note the run holding it makes
-- is above it.
ALTER TABLE index_lease ADD COLUMN IF NOT EXISTS noted_from BIGINT NOT NULL DEFAULT 0;
