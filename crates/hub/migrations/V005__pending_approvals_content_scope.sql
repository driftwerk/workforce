-- The guard of the turn that asked, when it had read outside content: the
-- turn resumed on the decision runs under it, whatever the session's other
-- turns did meanwhile. Rows from before resume unguarded, as they did.
ALTER TABLE pending_approvals ADD COLUMN IF NOT EXISTS content_scope TEXT;
