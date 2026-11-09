-- Where a gist's purpose came from, when a handoff carried it from another
-- session's gist (`CarryPurpose`): that first session's subject, kept
-- through every hand on. Null for a purpose the subject's own turns wrote.
-- Forgetting a session clears only the purposes carried from it.
ALTER TABLE floor.gists ADD COLUMN purpose_from TEXT;
