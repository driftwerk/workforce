-- Before claim grounding failed closed, a pass that checked nothing (or was
-- cut short) read as clean and promoted a deep-expertise node to 'verified'
-- for good. Nothing stored tells those passes from real ones, so every
-- machine-verified node goes back to 'modelled', where the deepening loop picks
-- it up and the quality gate must earn 'verified' again. 'approved' is a
-- person's call and is left alone. Kept in step with
-- DEMOTE_UNGROUNDED_VERIFIED in src/sqlite.rs.

UPDATE deep_nodes SET status = 'modelled' WHERE status = 'verified';
