-- What a collection asks the store for: the indexes it wants, whether it is
-- big enough to deserve a partition of its own, what it searches, and its
-- limits. A collection with no row here is `small` and declares nothing —
-- which is every collection that existed before this migration.
CREATE TABLE collections (
    partition_key TEXT        PRIMARY KEY,
    scope_kind    TEXT        NOT NULL,
    scope_id      TEXT        NOT NULL,
    collection    TEXT        NOT NULL,
    descriptor    JSONB       NOT NULL,
    revision      BIGINT      NOT NULL DEFAULT 1,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- `documents` becomes partitioned by collection, so a collection declared
-- large can be given a partition of its own with its own local indexes: a
-- small collection then never pays planning or write cost for a big one's
-- indexes, and dropping a huge collection becomes DETACH rather than an
-- unbounded DELETE.
--
-- Three Postgres constraints shape the key, all verified against 16:
-- LIST partitioning takes exactly one column; a generated column may not be
-- a partition key; and a unique constraint must contain every partition key
-- column. So the key is one plain column holding the three parts joined by
-- `/`, and the primary key is (partition_key, doc_id) — equivalent to the old
-- (scope_kind, scope_id, collection, doc_id), since the key encodes the first
-- three. `/` is the separator because validate_segment forbids it inside a
-- segment while permitting `:`, which would have made `a:b` + `c` and `a` +
-- `b:c` collide into one partition and one key namespace.
ALTER TABLE documents RENAME TO documents_unpartitioned;
ALTER INDEX documents_collection_updated RENAME TO documents_unpartitioned_collection_updated;
ALTER INDEX documents_body RENAME TO documents_unpartitioned_body;
ALTER INDEX documents_expiry RENAME TO documents_unpartitioned_expiry;

CREATE TABLE documents (
    partition_key    TEXT        NOT NULL,
    doc_id           TEXT        NOT NULL,
    scope_kind       TEXT        NOT NULL,
    scope_id         TEXT        NOT NULL,
    collection       TEXT        NOT NULL,
    body             JSONB       NOT NULL,
    -- Filled by the write path only where a descriptor declares text_search,
    -- and indexed only on the partitions that do. It lives on the parent
    -- because a partition may not carry a column the parent lacks.
    search_vec       TSVECTOR,
    version          BIGINT      NOT NULL DEFAULT 1,
    created_by       TEXT,
    created_at       TIMESTAMPTZ NOT NULL,
    updated_at       TIMESTAMPTZ NOT NULL,
    expires_at       TIMESTAMPTZ,
    lease_owner      TEXT,
    lease_expires_at TIMESTAMPTZ,
    PRIMARY KEY (partition_key, doc_id)
) PARTITION BY LIST (partition_key);

-- Every collection lives here until one is declared large and given its own.
CREATE TABLE documents_default PARTITION OF documents DEFAULT;

INSERT INTO documents (
    partition_key, doc_id, scope_kind, scope_id, collection, body, version,
    created_by, created_at, updated_at, expires_at, lease_owner, lease_expires_at
)
SELECT
    scope_kind || '/' || scope_id || '/' || collection,
    doc_id, scope_kind, scope_id, collection, body, version,
    created_by, created_at, updated_at, expires_at, lease_owner, lease_expires_at
FROM documents_unpartitioned;

DROP TABLE documents_unpartitioned;

-- Listing a collection newest-first stays the common read, now keyed by the
-- partition key so the planner can prune to one partition.
CREATE INDEX documents_collection_updated
    ON documents (partition_key, updated_at DESC, doc_id);
-- The blanket body index every collection still shares. Collections declared
-- large get their own targeted indexes instead; this one is what an
-- undeclared field falls back to.
CREATE INDEX documents_body
    ON documents USING GIN (body jsonb_path_ops);
CREATE INDEX documents_expiry
    ON documents (expires_at) WHERE expires_at IS NOT NULL;
