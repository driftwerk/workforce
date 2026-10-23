-- Shadow mode (DW-676): a store Weaviate serves is also asked of vellev in
-- the background, and the two answers compared. One row per compared
-- question, kept 14 days; one row per store and UTC day, kept 180 days,
-- rolled up from them before they go (crates/common/src/shadow_postgres.rs).
--
-- Nothing a user does waits on these tables: rows are written by one
-- writer task per hub, in batches, off every request's path.

CREATE TABLE IF NOT EXISTS shadow_comparisons (
    id                  BIGSERIAL PRIMARY KEY,
    store               TEXT NOT NULL,
    at                  TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- answered, failed, panicked or timed_out: how the shadow went.
    outcome             TEXT NOT NULL
        CHECK (outcome IN ('answered', 'failed', 'panicked', 'timed_out')),
    error               TEXT,
    -- The breaker's one try after a rest.
    probe               BOOLEAN NOT NULL,
    -- A hash of the question; its text is not kept.
    query_hash          TEXT,
    labels              JSONB NOT NULL DEFAULT '{}'::jsonb,
    -- Each side's index call alone, and its embedding apart (NULL when it
    -- was handed a vector).
    primary_ms          DOUBLE PRECISION NOT NULL,
    primary_embed_ms    DOUBLE PRECISION,
    primary_count       INTEGER NOT NULL,
    primary_top_score   REAL,
    shadow_ms           DOUBLE PRECISION NOT NULL,
    shadow_embed_ms     DOUBLE PRECISION,
    -- The rest only when the shadow answered.
    shadow_count        INTEGER,
    shadow_top_score    REAL,
    k                   INTEGER,
    overlap             DOUBLE PRECISION,
    jaccard             DOUBLE PRECISION,
    rbo                 DOUBLE PRECISION,
    top1                BOOLEAN,
    empty_primary_only  BOOLEAN,
    empty_shadow_only   BOOLEAN
);

CREATE INDEX IF NOT EXISTS shadow_comparisons_at ON shadow_comparisons (at);

-- A store's day, rolled up from its comparisons. Agreement, and the first
-- latency percentiles, are over the questions the shadow answered, so both
-- sides are timed on the same questions; the others are counted by
-- outcome.
CREATE TABLE IF NOT EXISTS shadow_daily (
    store               TEXT NOT NULL,
    day                 DATE NOT NULL,
    samples             INTEGER NOT NULL,
    answered            INTEGER NOT NULL,
    failed              INTEGER NOT NULL,
    panicked            INTEGER NOT NULL,
    timed_out           INTEGER NOT NULL,
    probes              INTEGER NOT NULL,
    mean_overlap        DOUBLE PRECISION,
    mean_jaccard        DOUBLE PRECISION,
    mean_rbo            DOUBLE PRECISION,
    top1_rate           DOUBLE PRECISION,
    empty_primary_only  INTEGER NOT NULL,
    empty_shadow_only   INTEGER NOT NULL,
    primary_p50_ms      DOUBLE PRECISION,
    primary_p95_ms      DOUBLE PRECISION,
    primary_p99_ms      DOUBLE PRECISION,
    shadow_p50_ms       DOUBLE PRECISION,
    shadow_p95_ms       DOUBLE PRECISION,
    shadow_p99_ms       DOUBLE PRECISION,
    -- The same, for the exit gate (L1): over the answered and the timed
    -- out, each timeout at its shadow_ms (about the deadline, a lower
    -- bound), so a slow tail is counted rather than left out. A week's
    -- p99 is worked out from shadow_comparisons, never from these.
    primary_p50_ms_with_timeouts DOUBLE PRECISION,
    primary_p95_ms_with_timeouts DOUBLE PRECISION,
    primary_p99_ms_with_timeouts DOUBLE PRECISION,
    shadow_p50_ms_with_timeouts  DOUBLE PRECISION,
    shadow_p95_ms_with_timeouts  DOUBLE PRECISION,
    shadow_p99_ms_with_timeouts  DOUBLE PRECISION,
    rolled_up_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (store, day)
);
