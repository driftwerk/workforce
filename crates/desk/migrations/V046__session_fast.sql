-- A Codex session started in Fast mode: it asks the ChatGPT backend's
-- faster tier (service_tier = "priority"), on every launch and resume of it.
ALTER TABLE sessions ADD COLUMN fast BOOLEAN NOT NULL DEFAULT FALSE;
