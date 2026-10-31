-- When each session last did something worth your attention: its turn
-- ended, it stopped unexpectedly, or it stopped or closed with nobody in
-- Desk asking it to. What it is unread by, rather than every change of its
-- state (your prompt, your Stop, a restart).
ALTER TABLE sessions ADD COLUMN attention_at TIMESTAMPTZ;
UPDATE sessions SET attention_at = state_at;
