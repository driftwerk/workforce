-- Run identity for member-owned definitions (docs/workflow-run-identity-design.md).
--
-- An automated run (webhook, schedule, HTTP trigger) executes as this identity
-- when set, else as the owner. Only the saving identity may name itself here,
-- so a manage-share grantee can take responsibility for the automation of a
-- workflow they co-maintain without the owner re-approving every edit.
ALTER TABLE workflow_defs ADD COLUMN IF NOT EXISTS run_as_identity_id TEXT;
