-- Session memory's notes in the search index (SM3): how often the index
-- worker tried a note that failed on its own, as for exchanges, and what it
-- writes next (only notes still held are in the index).
ALTER TABLE memory_items ADD COLUMN index_tries INT NOT NULL DEFAULT 0;

CREATE INDEX memory_items_pending ON memory_items (written_at)
    WHERE indexed_at IS NULL AND valid_to IS NULL;

-- Notes found by their words while the index isn't ready.
CREATE INDEX memory_items_tsv ON memory_items USING GIN (to_tsvector('simple', text));
