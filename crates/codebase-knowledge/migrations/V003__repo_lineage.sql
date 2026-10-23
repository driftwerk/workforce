-- Tag repository rows by the on-disk layout that produced them.
-- Computed at the hub layer (the only place that knows the canonical
-- post-DW-506 paths) and persisted here so `knowledge.delete prune`
-- and `knowledge.status` can filter without rerunning the classifier.
--
-- Default is 'unknown' so existing rows survive the migration; the
-- hub backfill task classifies them on next startup.

ALTER TABLE repositories
    ADD COLUMN IF NOT EXISTS lineage TEXT NOT NULL DEFAULT 'unknown'
        CHECK (lineage IN ('canonical', 'migration_orphan', 'unknown'));

CREATE INDEX IF NOT EXISTS idx_repositories_lineage ON repositories(lineage);
