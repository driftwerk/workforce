-- What waits on an approver is read by their id on every floor read.
CREATE INDEX IF NOT EXISTS idx_pending_approval_targets_approver
    ON pending_approval_targets (approver_id);
