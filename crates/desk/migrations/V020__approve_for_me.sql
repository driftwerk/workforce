-- A Codex session started with "Approve for me": Codex's own reviewer
-- answers its approvals (approvals_reviewer = auto_review), on every launch
-- and resume of it.
ALTER TABLE sessions ADD COLUMN approve_for_me BOOLEAN NOT NULL DEFAULT FALSE;
