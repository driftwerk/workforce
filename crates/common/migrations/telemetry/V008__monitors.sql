-- Channel monitors: durability state. Definitions themselves live in
-- `workflow_defs` under kind = 'monitor' (revisioned via `def_revisions`),
-- mirroring workflows and playbooks; only the runtime state lives here.

-- Per watched (surface, channel): how far the catch-up sweep has read.
-- The cursor is a READ OPTIMISATION ONLY — `monitor_dispatches` is the source
-- of truth, so the cursor may advance freely without stranding work.
CREATE TABLE IF NOT EXISTS monitor_channel_cursors (
    surface        TEXT NOT NULL,
    channel        TEXT NOT NULL,
    last_swept_ts  TEXT NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (surface, channel)
);

-- The idempotency gate AND the durable work queue. One row per
-- (monitor, message) dispatch. `payload` is persisted at claim time so an
-- incomplete dispatch is recovered directly (no channel re-read). Only
-- completed rows are pruned by retention; incomplete rows survive regardless
-- of age so pending work is never dropped.
CREATE TABLE IF NOT EXISTS monitor_dispatches (
    monitor_id    TEXT NOT NULL,
    owner         TEXT NOT NULL DEFAULT '__system__',
    surface       TEXT NOT NULL,
    workspace     TEXT NOT NULL DEFAULT '',
    channel       TEXT NOT NULL,
    message_id    TEXT NOT NULL,
    payload       JSONB NOT NULL,
    claimed_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at  TIMESTAMPTZ,
    instance_id   TEXT,
    error         TEXT,
    PRIMARY KEY (monitor_id, owner, surface, channel, message_id)
);

CREATE INDEX IF NOT EXISTS idx_monitor_dispatches_recover
    ON monitor_dispatches (completed_at, claimed_at);
CREATE INDEX IF NOT EXISTS idx_monitor_dispatches_activity
    ON monitor_dispatches (monitor_id, owner, claimed_at DESC);
