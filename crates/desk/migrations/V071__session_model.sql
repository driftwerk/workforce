-- The model a session runs, as its engine names it (`opus`, `gpt-6.1-sol`),
-- on every launch and resume of it; null is the engine's own default.
ALTER TABLE sessions ADD COLUMN model TEXT;
