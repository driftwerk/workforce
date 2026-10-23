-- A trigger's public address, decoupled from who owns the workflow.
--
-- Before this, the public path was built from the owner
-- (`{owner}/{trigger_id}`), so giving a workflow away moved its URL and every
-- external caller 404'd. `public_path` is minted once, per (definition,
-- trigger), and never recomputed: a transfer moves `owner_identity_id` and the
-- URL does not move. The legacy owner-derived paths keep resolving; they are
-- derived from the loaded definitions and are deliberately not stored here.
--
-- Rows are insert-only. Deleting a definition does NOT delete its row (no
-- foreign key, by design): recreating the same definition under the same owner
-- revives the same URL, and a different owner can never inherit one because
-- `owner_identity_id` is part of the binding.
CREATE TABLE IF NOT EXISTS trigger_endpoints (
    public_path       TEXT PRIMARY KEY,
    kind              TEXT NOT NULL,
    def_id            TEXT NOT NULL,
    owner_identity_id TEXT NOT NULL DEFAULT '__system__',
    trigger_id        TEXT NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT trigger_endpoints_binding_key
        UNIQUE (kind, def_id, owner_identity_id, trigger_id)
);

CREATE INDEX IF NOT EXISTS idx_trigger_endpoints_def
    ON trigger_endpoints (kind, def_id, owner_identity_id);
