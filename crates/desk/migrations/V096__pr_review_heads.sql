-- The head each review session was started at, the session whose work the
-- pull request is, and when Desk was done with it (DW-765): Desk starts one
-- review a head, records its verdict, and keeps it open until it has.
-- Additive.
ALTER TABLE pr_review_sessions ADD COLUMN IF NOT EXISTS head_sha TEXT;
ALTER TABLE pr_review_sessions ADD COLUMN IF NOT EXISTS for_session UUID;
ALTER TABLE pr_review_sessions ADD COLUMN IF NOT EXISTS settled_at TIMESTAMPTZ;
CREATE INDEX IF NOT EXISTS pr_review_sessions_for ON pr_review_sessions (for_session);
