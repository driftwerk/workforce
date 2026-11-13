-- The boss's update, laid out (overseer H7): each decision says what was
-- done in a short line and why in one sentence, keeps the full message it
-- sent a session apart from both, and says whether it acted or left the
-- session alone, so the chat can fold the times it did nothing. An ask
-- keeps its one-line question, its points, the answer recommended and what
-- carries on meanwhile. Additive: older rows read with the defaults.
ALTER TABLE boss_decisions ADD COLUMN brief TEXT NOT NULL DEFAULT '';
ALTER TABLE boss_decisions ADD COLUMN brief_why TEXT NOT NULL DEFAULT '';
-- The full text sent to the session; none when nothing was sent.
ALTER TABLE boss_decisions ADD COLUMN message TEXT;
-- acted | left_alone | escalated | undone
ALTER TABLE boss_decisions ADD COLUMN kind TEXT NOT NULL DEFAULT 'acted';

-- Older rows: a turn it let wait, or a line it sent nothing to, was left
-- alone; a question given to the person was escalated; the person's undo
-- is an undo. A line message's words are its message.
UPDATE boss_decisions SET kind = 'left_alone'
    WHERE need = 'turn_end' OR (need = 'choice' AND what LIKE 'sent % nothing');
UPDATE boss_decisions SET kind = 'escalated' WHERE how = 'escalated';
UPDATE boss_decisions SET kind = 'undone' WHERE need = 'undo';
UPDATE boss_decisions SET message = action->>'text'
    WHERE jsonb_typeof(action) = 'object' AND action->>'kind' = 'line';

ALTER TABLE boss_open_items ADD COLUMN question TEXT;
ALTER TABLE boss_open_items ADD COLUMN points JSONB NOT NULL DEFAULT '[]';
-- The option it recommends, one of `options`.
ALTER TABLE boss_open_items ADD COLUMN recommended TEXT;
ALTER TABLE boss_open_items ADD COLUMN not_blocking TEXT;
