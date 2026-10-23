-- Shadow mode for memories (DW-676): vellev is kept a replica of Weaviate
-- by a reconcile the leader runs at a fixed interval, and each run records
-- what it had to fix, by memory, so that every fix can be traced to how it
-- was missed (a write that bypassed the mirror, a decay job, a dropped
-- note). Kept 180 days by the leader's retention pass.
--
-- V030: V026 (monitors) and V027 (workflow instance version) are main's,
-- V028 the shadow comparisons (PR 3) and V029 the artifact mirror (PR 4).
-- Refinery refuses a lower version found after a higher one was applied,
-- so this merges after V029.

CREATE TABLE IF NOT EXISTS shadow_reconciles (
    id              BIGSERIAL PRIMARY KEY,
    store           TEXT NOT NULL,
    started_at      TIMESTAMPTZ NOT NULL,
    finished_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- ok; failed; or refused: the replica promoted, the store Weaviate
    -- serves not yet the one the replica copies, or the replica's tenant
    -- holding a collection it must not.
    outcome         TEXT NOT NULL
        CHECK (outcome IN ('ok', 'failed', 'refused_promoted', 'refused_unswitched', 'refused_tenant')),
    error           TEXT,
    copied          INTEGER NOT NULL DEFAULT 0,
    updated         INTEGER NOT NULL DEFAULT 0,
    dropped         INTEGER NOT NULL DEFAULT 0,
    unchanged       INTEGER NOT NULL DEFAULT 0,
    -- The first fixes, each {"kind", "object_id", "memory_id"}; how many
    -- more there were is in fixed_left_out.
    fixed           JSONB NOT NULL DEFAULT '[]'::jsonb,
    fixed_left_out  INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS shadow_reconciles_finished_at ON shadow_reconciles (finished_at);

-- That memories have been promoted to vellev: written by promotion
-- (`workforce shadow promote`), and read by every write into the replica
-- beside vellev's own marker, which a restore of the tenant to an earlier
-- point can take away. At most one row.
CREATE TABLE IF NOT EXISTS shadow_memory_promotion (
    one             BOOLEAN PRIMARY KEY DEFAULT true CHECK (one),
    collection      TEXT NOT NULL,
    promoted_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
