-- Artifacts v2 schema (DW-547, epic DW-546).
--
-- Promotes artifacts from flat per-agent files (`SharedWorkspace`,
-- `crates/orchestrator/src/workspace.rs`) to a first-class, DB-backed
-- knowledge base with per-user/global ACL, tags, and a `snippet` type.
-- Also moves workflow & playbook *definitions* off disk (`workflow_defs`);
-- workflow *instances* already live in `workflow_instances` (V001).
--
-- Lives in the `telemetry` schema alongside the rest of the Postgres
-- store (the migration runner pins `search_path` before applying).
-- Renumbered to V005 — V002..V004 are already taken; refinery enforces
-- sequential, checksummed versions and will not re-run an applied file,
-- so `IF NOT EXISTS` is belt-and-braces rather than the real guard.

CREATE TABLE IF NOT EXISTS artifacts (
    id               TEXT PRIMARY KEY,
    artifact_type    TEXT NOT NULL,
    title            TEXT NOT NULL,
    content          TEXT NOT NULL,
    owner_id         TEXT NOT NULL,
    visibility       TEXT NOT NULL DEFAULT 'private',
    tags             TEXT NOT NULL DEFAULT '[]',
    status           TEXT NOT NULL DEFAULT 'draft',
    version          INTEGER NOT NULL DEFAULT 1,
    created_by       TEXT NOT NULL,
    last_modified_by TEXT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL,
    updated_at       TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_artifacts_owner ON artifacts(owner_id);
CREATE INDEX IF NOT EXISTS idx_artifacts_visibility ON artifacts(visibility);
CREATE INDEX IF NOT EXISTS idx_artifacts_type ON artifacts(artifact_type);
CREATE INDEX IF NOT EXISTS idx_artifacts_updated ON artifacts(updated_at);

-- Per-user read grants for `private` artifacts (the `share` action).
-- `public`/`team` visibility is open and does not need a row here.
CREATE TABLE IF NOT EXISTS artifact_shares (
    artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    user_id     TEXT NOT NULL,
    granted_at  TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (artifact_id, user_id)
);
CREATE INDEX IF NOT EXISTS idx_artifact_shares_user ON artifact_shares(user_id);

-- Workflow & playbook definitions (DW-550). Keyed by (id, agent_id):
-- definitions become per-agent, matching how the runtime scopes them.
-- `kind` distinguishes 'workflow' from 'playbook'. `parsed_json` caches
-- the validated TOML so loads don't re-parse.
CREATE TABLE IF NOT EXISTS workflow_defs (
    id          TEXT NOT NULL,
    agent_id    TEXT NOT NULL,
    kind        TEXT NOT NULL DEFAULT 'workflow',
    toml_source TEXT NOT NULL,
    parsed_json JSONB,
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (id, agent_id)
);
CREATE INDEX IF NOT EXISTS idx_workflow_defs_agent ON workflow_defs(agent_id);
CREATE INDEX IF NOT EXISTS idx_workflow_defs_kind ON workflow_defs(kind);
CREATE INDEX IF NOT EXISTS idx_workflow_defs_updated ON workflow_defs(updated_at);
