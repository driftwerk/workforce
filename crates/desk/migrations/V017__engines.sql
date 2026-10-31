-- Where each session runs (a terminal, or Desk's own view of it) and the
-- engine's version when it last started: what Desk can do with it follows
-- from both.
ALTER TABLE sessions ADD COLUMN view TEXT NOT NULL DEFAULT 'terminal',
    ADD COLUMN engine_version TEXT;
