-- Workflow & playbook ownership (workflow-ownership epic, P1).
--
-- Definitions gain an owner identity and a visibility. Ownership lets any
-- member create/run/share their own definitions; execution identity is derived
-- from ownership (never author-declared). Existing rows predate ownership and
-- were admin-deployed + globally runnable, so they migrate to the system owner
-- with global visibility to preserve behaviour.
--
-- The primary key moves from (id, agent_id) to (id, owner_identity_id) so two
-- members may each own a definition with the same id without collision.
-- `agent_id` is retained (always '_global') but no longer part of the key.

ALTER TABLE workflow_defs
    ADD COLUMN IF NOT EXISTS owner_identity_id TEXT NOT NULL DEFAULT '__system__',
    ADD COLUMN IF NOT EXISTS visibility TEXT NOT NULL DEFAULT 'private';

-- Existing rows were globally runnable before ownership existed.
UPDATE workflow_defs SET visibility = 'global' WHERE owner_identity_id = '__system__';

ALTER TABLE workflow_defs DROP CONSTRAINT IF EXISTS workflow_defs_pkey;
ALTER TABLE workflow_defs ADD PRIMARY KEY (id, owner_identity_id);

CREATE INDEX IF NOT EXISTS idx_workflow_defs_owner ON workflow_defs(owner_identity_id);
CREATE INDEX IF NOT EXISTS idx_workflow_defs_visibility ON workflow_defs(visibility);
