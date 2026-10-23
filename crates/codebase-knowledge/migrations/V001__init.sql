-- Code graph store schema (Postgres backend, Phase 3).
--
-- Translation of `crates/codebase-knowledge/src/graph/schema.rs`.
-- All tables live in the schema named when this set is registered
-- (`codegraph` by default — see `MigrationSet` in
-- `crates/postgres/src/migrations.rs`). The migration runner sets
-- `search_path` before applying.
--
-- Type translations from SQLite:
--   TEXT (id)                  →  TEXT  (we keep entity IDs as TEXT
--                                 because they are deterministic
--                                 blake3-hash hex strings assigned by
--                                 the indexer, not database-generated)
--   TEXT (datetime('now'))     →  TIMESTAMPTZ DEFAULT now()
--   INTEGER                    →  BIGINT (line numbers, counts, flags)
--   REAL                       →  DOUBLE PRECISION (confidence)
--   INTEGER (bool 0/1)         →  BOOLEAN
--   TEXT (JSON in metadata)    →  JSONB

CREATE TABLE IF NOT EXISTS repositories (
    id                  TEXT PRIMARY KEY,
    name                TEXT NOT NULL,
    root_path           TEXT NOT NULL,
    last_indexed_commit TEXT,
    last_indexed_at     TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Code entities (functions, structs, files, modules, etc.).
--
-- `parent_id` is a self-referencing foreign key and is checked with
-- deferred constraints during batch upserts (children can be inserted
-- before their parents within a single `upsert_entities` batch).
CREATE TABLE IF NOT EXISTS entities (
    id              TEXT PRIMARY KEY,
    repo_id         TEXT NOT NULL REFERENCES repositories(id) DEFERRABLE INITIALLY DEFERRED,
    kind            TEXT NOT NULL,
    name            TEXT NOT NULL,
    qualified_name  TEXT NOT NULL,
    file_path       TEXT NOT NULL,
    start_line      BIGINT NOT NULL,
    end_line        BIGINT NOT NULL,
    signature       TEXT,
    visibility      TEXT NOT NULL DEFAULT 'private',
    parent_id       TEXT REFERENCES entities(id) DEFERRABLE INITIALLY DEFERRED,
    doc_comment     TEXT,
    content_hash    TEXT NOT NULL,
    language        TEXT NOT NULL,
    indexed_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_entities_repo ON entities(repo_id);
CREATE INDEX IF NOT EXISTS idx_entities_kind ON entities(kind);
CREATE INDEX IF NOT EXISTS idx_entities_file ON entities(file_path);
CREATE INDEX IF NOT EXISTS idx_entities_name ON entities(name);
CREATE INDEX IF NOT EXISTS idx_entities_qualified ON entities(qualified_name);
-- Composite index so `delete_repo_data` / `all_entities` / `find_entities_in_file`
-- can push the repo_id filter down to the index instead of the heap.
CREATE INDEX IF NOT EXISTS idx_entities_repo_file
    ON entities(repo_id, file_path);

CREATE TABLE IF NOT EXISTS relationships (
    id          TEXT PRIMARY KEY,
    source_id   TEXT NOT NULL REFERENCES entities(id) DEFERRABLE INITIALLY DEFERRED,
    target_id   TEXT NOT NULL REFERENCES entities(id) DEFERRABLE INITIALLY DEFERRED,
    kind        TEXT NOT NULL,
    confidence  DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    metadata    JSONB,
    indexed_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_rel_source ON relationships(source_id);
CREATE INDEX IF NOT EXISTS idx_rel_target ON relationships(target_id);
CREATE INDEX IF NOT EXISTS idx_rel_kind ON relationships(kind);
-- Composite index so `neighbours()` (which filters on source_id+kind
-- or target_id+kind) hits an index directly.
CREATE INDEX IF NOT EXISTS idx_rel_source_kind ON relationships(source_id, kind);
CREATE INDEX IF NOT EXISTS idx_rel_target_kind ON relationships(target_id, kind);

CREATE TABLE IF NOT EXISTS summaries (
    entity_id    TEXT NOT NULL REFERENCES entities(id) DEFERRABLE INITIALLY DEFERRED,
    granularity  TEXT NOT NULL,
    text         TEXT NOT NULL,
    source_hash  TEXT NOT NULL,
    model_used   TEXT NOT NULL,
    generated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    is_stale     BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (entity_id, granularity)
);
CREATE INDEX IF NOT EXISTS idx_summaries_stale ON summaries(is_stale) WHERE is_stale = TRUE;

CREATE TABLE IF NOT EXISTS index_state (
    repo_id                 TEXT PRIMARY KEY REFERENCES repositories(id) DEFERRABLE INITIALLY DEFERRED,
    last_commit             TEXT,
    last_full_index_at      TIMESTAMPTZ,
    last_incremental_at     TIMESTAMPTZ,
    total_entities          BIGINT NOT NULL DEFAULT 0,
    total_relationships     BIGINT NOT NULL DEFAULT 0,
    indexing_in_progress    BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS file_index (
    repo_id         TEXT NOT NULL,
    file_path       TEXT NOT NULL,
    content_hash    TEXT NOT NULL,
    language        TEXT NOT NULL,
    entity_count    BIGINT NOT NULL DEFAULT 0,
    last_indexed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repo_id, file_path)
);
