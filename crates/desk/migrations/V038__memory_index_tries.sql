-- How often the index worker tried an exchange that failed on its own:
-- one that keeps failing is set aside, so it can't hold up the rest.
ALTER TABLE memory_exchanges ADD COLUMN index_tries INT NOT NULL DEFAULT 0;

-- An index deletion that kept failing while the index answered otherwise:
-- set aside, and said on the Floor, until a rebuild clears it.
ALTER TABLE memory_purges ADD COLUMN set_aside_at TIMESTAMPTZ;

-- V037's memory_exchanges_pending does what this did.
DROP INDEX IF EXISTS memory_exchanges_outbox;

-- Every sidebar group id there has ever been: one is never used again, so
-- rows stamped with a deleted group never become another group's.
CREATE TABLE group_ids_used (
    id         TEXT        PRIMARY KEY,
    first_used TIMESTAMPTZ NOT NULL
);
INSERT INTO group_ids_used (id, first_used)
SELECT id, now() FROM groups WHERE id <> 'ungrouped'
UNION SELECT DISTINCT group_id, now() FROM memory_exchanges WHERE group_id IS NOT NULL
UNION SELECT DISTINCT group_id, now() FROM ledger WHERE group_id IS NOT NULL
ON CONFLICT DO NOTHING;
