-- Pending notes' marks, as SQLite's schema v7 (see schema.rs).
CREATE TABLE IF NOT EXISTS vector_note_mark (
    only_one INTEGER PRIMARY KEY CHECK (only_one = 1),
    last     BIGINT NOT NULL
);

INSERT INTO vector_note_mark (only_one, last)
SELECT 1, COALESCE(max(seq), 0) FROM vector_pending
ON CONFLICT (only_one) DO NOTHING;

ALTER TABLE index_lease ADD COLUMN IF NOT EXISTS noted_from BIGINT NOT NULL DEFAULT 0;
