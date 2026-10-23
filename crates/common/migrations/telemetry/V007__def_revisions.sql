-- Revision history for definitions (revisions-and-config-overlay design, Part A).
--
-- One table for every revisioned asset kind:
--   'workflow' | 'playbook'      ref_id = definition id (operative row in workflow_defs)
--   'identity_file'              ref_id = '{agent_id}/{filename}' (operative file on disk)
--   'config_overlay'             ref_id = 'global' (operative file = workforce.overlay.toml)
--
-- Every save inserts a row (skipped when content is byte-identical to the
-- newest revision); delete inserts a tombstone (deleted = TRUE, empty content)
-- so "who deleted it" survives the operative row's removal. Restore never
-- rewrites history: it inserts a NEW revision whose content equals the chosen
-- one, with edited_via = 'restore' and restored_from pointing at the source.

CREATE TABLE IF NOT EXISTS def_revisions (
    kind              TEXT NOT NULL,
    ref_id            TEXT NOT NULL,
    owner_identity_id TEXT NOT NULL DEFAULT '__system__',
    rev               INTEGER NOT NULL,
    content           TEXT NOT NULL,
    content_hash      TEXT NOT NULL,
    deleted           BOOLEAN NOT NULL DEFAULT FALSE,
    edited_by         TEXT,
    edited_via        TEXT NOT NULL,
    restored_from     INTEGER,
    detail            TEXT,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, ref_id, owner_identity_id, rev)
);

CREATE INDEX IF NOT EXISTS idx_def_revisions_lookup
    ON def_revisions (kind, ref_id, owner_identity_id, rev DESC);

-- Display convenience on the operative row; def_revisions is the source of truth.
ALTER TABLE workflow_defs ADD COLUMN IF NOT EXISTS last_modified_by TEXT;
