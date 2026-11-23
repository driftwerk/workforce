-- The automated origin the turn that asked carried: the turn resumed on its
-- decision carries it too. Rows from before resume as they always did.
ALTER TABLE pending_approvals ADD COLUMN IF NOT EXISTS origin TEXT;
