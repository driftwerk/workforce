-- Per-agent training corpora — DW-539 Phase 1a.
-- Tables live in the `training` schema (see PostgresTrainingStore::SCHEMA).
-- Type translations match the SQLite schema in src/sqlite.rs.

CREATE TABLE IF NOT EXISTS training_corpora (
    id              UUID        PRIMARY KEY,
    agent_id        TEXT        NOT NULL,
    name            TEXT        NOT NULL,
    scope_kind      TEXT        NOT NULL DEFAULT 'agent',
    scope_id        TEXT,
    created_at      TIMESTAMPTZ NOT NULL,
    created_by      TEXT        NOT NULL,
    UNIQUE (agent_id, name)
);

CREATE INDEX IF NOT EXISTS idx_training_corpora_agent
    ON training_corpora(agent_id);

CREATE TABLE IF NOT EXISTS training_documents (
    id                  UUID        PRIMARY KEY,
    corpus_id           UUID        NOT NULL REFERENCES training_corpora(id) ON DELETE CASCADE,
    original_filename   TEXT        NOT NULL,
    mime_type           TEXT        NOT NULL,
    size_bytes          BIGINT      NOT NULL,
    storage_uri         TEXT        NOT NULL,
    source_kind         TEXT        NOT NULL,
    source_ref          TEXT,
    processing_status   TEXT        NOT NULL DEFAULT 'pending',
    processing_error    TEXT,
    uploaded_at         TIMESTAMPTZ NOT NULL,
    uploaded_by         TEXT        NOT NULL,
    page_count          INTEGER
);

CREATE INDEX IF NOT EXISTS idx_training_documents_corpus
    ON training_documents(corpus_id);
CREATE INDEX IF NOT EXISTS idx_training_documents_status
    ON training_documents(processing_status);

CREATE TABLE IF NOT EXISTS training_chunks (
    id              UUID        PRIMARY KEY,
    document_id     UUID        NOT NULL REFERENCES training_documents(id) ON DELETE CASCADE,
    chunk_index     INTEGER     NOT NULL,
    content_hash    TEXT        NOT NULL,
    memory_id       TEXT        NOT NULL,
    locator         TEXT        NOT NULL DEFAULT '',
    char_start      INTEGER     NOT NULL DEFAULT 0,
    char_end        INTEGER     NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_training_chunks_document
    ON training_chunks(document_id);
CREATE INDEX IF NOT EXISTS idx_training_chunks_hash
    ON training_chunks(content_hash);
CREATE UNIQUE INDEX IF NOT EXISTS idx_training_chunks_doc_index
    ON training_chunks(document_id, chunk_index);
