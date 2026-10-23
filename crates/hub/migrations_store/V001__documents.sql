-- One row per document. A document is addressed by its scope (who may see
-- it), a collection (what kind of thing it is) and an id; the body is
-- whatever JSON the writer chose, so the schema survives every consumer's
-- shape evolving. `version` bumps on every write and is the compare-and-swap
-- token; the lease columns are the `claim` op (a worker takes a document for
-- a while, the way execute_loop leases a workflow instance).
CREATE TABLE documents (
    scope_kind       TEXT        NOT NULL,   -- user | agent | global
    scope_id         TEXT        NOT NULL,   -- identity id | agent id | '' for global
    collection       TEXT        NOT NULL,
    doc_id           TEXT        NOT NULL,
    body             JSONB       NOT NULL,
    version          BIGINT      NOT NULL DEFAULT 1,
    created_by       TEXT,
    created_at       TIMESTAMPTZ NOT NULL,
    updated_at       TIMESTAMPTZ NOT NULL,
    expires_at       TIMESTAMPTZ,
    lease_owner      TEXT,
    lease_expires_at TIMESTAMPTZ,
    PRIMARY KEY (scope_kind, scope_id, collection, doc_id)
);

-- Listing a collection newest-first and querying inside a body are the two
-- reads every consumer makes.
CREATE INDEX documents_collection_updated
    ON documents (scope_kind, scope_id, collection, updated_at DESC);
CREATE INDEX documents_body
    ON documents USING GIN (body jsonb_path_ops);
CREATE INDEX documents_expiry
    ON documents (expires_at) WHERE expires_at IS NOT NULL;
