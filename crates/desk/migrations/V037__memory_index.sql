-- Session memory's search index (SM2). Exchanges past a session's cap of
-- 20 000 stay in Postgres, found by keyword, but are taken out of the index
-- and never written to it again.
ALTER TABLE memory_exchanges ADD COLUMN past_cap BOOLEAN NOT NULL DEFAULT false;

-- What the index worker writes next: closed exchanges not yet in vellev.
CREATE INDEX memory_exchanges_pending ON memory_exchanges (at)
    WHERE indexed_at IS NULL AND ended IS NOT NULL AND NOT past_cap;

-- Index deletions still to run, oldest first.
CREATE INDEX memory_purges_pending ON memory_purges (created_at) WHERE done_at IS NULL;
