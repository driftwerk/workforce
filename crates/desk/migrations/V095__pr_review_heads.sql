-- The head each review session was started at, and the session whose work
-- the pull request is (DW-765): Desk starts one review a head, and records
-- and closes it from the plan watch of that session's ticket. Additive.
ALTER TABLE pr_review_sessions ADD COLUMN IF NOT EXISTS head_sha TEXT;
ALTER TABLE pr_review_sessions ADD COLUMN IF NOT EXISTS for_session UUID;
CREATE INDEX IF NOT EXISTS pr_review_sessions_for ON pr_review_sessions (for_session);
