-- Shadow mode's known-item runs (DW-676, PR 10): each night the leader
-- draws memories (or artifacts) from the store Weaviate serves, by a seed
-- and the date, makes a query of each, and asks both stores whether it
-- comes back first.
-- A run is one row; each item, at each hybrid alpha, another. No query
-- text is kept: it is made again from the run's seed and the memory.
-- Kept 180 days by the leader's retention pass, items with their run.
--
-- Never applied anywhere before it merged (only to throwaway test
-- databases), so it was edited in place in review.
--
-- V032: V031 is the memory mirror's notes (PR 7b). Refinery refuses a
-- lower version found after a higher one was applied, so this merges after
-- V031.

CREATE TABLE IF NOT EXISTS shadow_known_item_runs (
    id              BIGSERIAL PRIMARY KEY,
    store           TEXT NOT NULL,
    -- The UTC day the run is for, which with base_seed makes its seed.
    run_day         DATE NOT NULL,
    base_seed       BIGINT NOT NULL,
    -- The seed the draw and the queries are made from, as hex.
    run_seed        TEXT NOT NULL,
    -- How a query is made from a memory; changed only with the code.
    derivation      INTEGER NOT NULL,
    started_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at     TIMESTAMPTZ,
    -- running until it ends; ok, every item drawn asked; incomplete,
    -- stopped part way (paused, no longer the leader, vellev down);
    -- failed, nothing drawn; abandoned, found still running long after
    -- its hub must have gone.
    outcome         TEXT NOT NULL DEFAULT 'running'
        CHECK (outcome IN ('running', 'ok', 'incomplete', 'failed', 'abandoned')),
    error           TEXT,
    -- Depth each side's answer is read to.
    k               INTEGER NOT NULL,
    -- The serving store's own hybrid alpha, asked beside 1.0.
    alpha_serving   REAL NOT NULL,
    items_wanted    INTEGER NOT NULL,
    -- Once drawn: the memories looked at, those left out by reason, and
    -- those drawn.
    population      INTEGER,
    excluded        JSONB,
    drawn           INTEGER,
    -- Of those drawn, memories filed under no identity, recalled unscoped.
    drawn_unscoped  INTEGER
);

-- One run a store and day that is running or ran to its end: whichever hub
-- leads, and however often it restarts.
CREATE UNIQUE INDEX IF NOT EXISTS shadow_known_item_runs_one_a_day
    ON shadow_known_item_runs (store, run_day)
    WHERE outcome IN ('running', 'ok');

CREATE INDEX IF NOT EXISTS shadow_known_item_runs_started_at
    ON shadow_known_item_runs (started_at);

CREATE TABLE IF NOT EXISTS shadow_known_items (
    id              BIGSERIAL PRIMARY KEY,
    run_id          BIGINT NOT NULL REFERENCES shadow_known_item_runs (id) ON DELETE CASCADE,
    -- The item: a memory's id, or an artifact's. By id alone, never the
    -- identity it is filed under: that would be a record about a person
    -- that erasure would have to reach.
    item_id         TEXT NOT NULL,
    alpha           REAL NOT NULL,
    -- scoped: a memory recalled within its identity (or an artifact);
    -- unscoped: a memory filed under no identity, recalled as system jobs
    -- recall, across every identity.
    population      TEXT NOT NULL DEFAULT 'scoped'
        CHECK (population IN ('scoped', 'unscoped')),
    -- window: half the item's words; hard: 4 to 8 from its middle.
    mode            TEXT NOT NULL DEFAULT 'window' CHECK (mode IN ('window', 'hard')),
    -- asked; embed_failed, its query could not be embedded and neither
    -- side was asked; no_permit, the shadow's permits stayed taken.
    status          TEXT NOT NULL CHECK (status IN ('asked', 'embed_failed', 'no_permit')),
    -- Each side: where the memory came back (1 is first; NULL, not within
    -- k), how many came back, how long the index call took, and the error
    -- in place of the rest. An error is never a miss.
    primary_rank    INTEGER,
    primary_count   INTEGER,
    primary_ms      DOUBLE PRECISION,
    primary_error   TEXT,
    -- Whether the item was found as a memory of the same content (a
    -- duplicate) rather than by its own id.
    primary_by_duplicate BOOLEAN,
    shadow_rank     INTEGER,
    shadow_count    INTEGER,
    shadow_ms       DOUBLE PRECISION,
    shadow_error    TEXT,
    shadow_by_duplicate BOOLEAN,
    -- The two answers compared, when both came.
    overlap         DOUBLE PRECISION,
    jaccard         DOUBLE PRECISION,
    rbo             DOUBLE PRECISION,
    top1            BOOLEAN,
    -- The opt-in judge's nDCG@5 of each side, for the artifacts' items it
    -- judged ([vellev.shadow.judge]); NULL otherwise.
    primary_ndcg    DOUBLE PRECISION,
    shadow_ndcg     DOUBLE PRECISION
);

CREATE INDEX IF NOT EXISTS shadow_known_items_run ON shadow_known_items (run_id);
