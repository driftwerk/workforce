-- What each Metabase instance's databases contain, so the native-query tool
-- schema can carry real table and column names instead of the model guessing.
--
-- `instances.site_url` is identity: a client whose resolved URL disagrees reads
-- nothing rather than another instance's names.

CREATE TABLE IF NOT EXISTS instances (
    instance  TEXT        PRIMARY KEY,
    site_url  TEXT        NOT NULL,
    synced_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One row per database: the name list is read and written whole, and is small.
CREATE TABLE IF NOT EXISTS databases (
    instance    TEXT        NOT NULL,
    database_id BIGINT      NOT NULL,
    tables      JSONB       NOT NULL,
    columns     JSONB       NOT NULL,
    synced_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (instance, database_id)
);

-- Per table, because counters must increment atomically: concurrent hub
-- replicas add deltas rather than writing a snapshot that drops the other's.
CREATE TABLE IF NOT EXISTS table_usage (
    instance    TEXT   NOT NULL,
    database_id BIGINT NOT NULL,
    table_name  TEXT   NOT NULL,
    hits        BIGINT NOT NULL DEFAULT 0,
    misses      BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (instance, database_id, table_name)
);
