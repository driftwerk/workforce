-- Schema corrections (the name that does not exist, beside the real one it
-- names in `tables`/`columns`), code locations (repo, path, symbol), and
-- re-checks: an entry whose names a re-check could not find is stale and not
-- shown until they are found again.
ALTER TABLE entries ADD COLUMN IF NOT EXISTS wrong TEXT;
ALTER TABLE entries ADD COLUMN IF NOT EXISTS repo TEXT;
ALTER TABLE entries ADD COLUMN IF NOT EXISTS path TEXT;
ALTER TABLE entries ADD COLUMN IF NOT EXISTS symbol TEXT;
ALTER TABLE entries ADD COLUMN IF NOT EXISTS stale BOOLEAN NOT NULL DEFAULT FALSE;
CREATE INDEX IF NOT EXISTS entries_last_seen ON entries (last_seen) WHERE NOT pinned AND NOT retired;
