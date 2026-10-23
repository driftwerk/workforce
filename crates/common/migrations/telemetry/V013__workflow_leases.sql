-- Cross-instance execution ownership for workflow instances: an execute loop
-- claims the row and renews its lease while running; recovery only claims
-- expired leases, and full-row saves are fenced on the owner.
ALTER TABLE workflow_instances ADD COLUMN IF NOT EXISTS owner_instance TEXT;
ALTER TABLE workflow_instances ADD COLUMN IF NOT EXISTS lease_expires_at TIMESTAMPTZ;
