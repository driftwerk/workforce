-- An agent's current conversation is read from its turns of the last hours
-- (its gist's purpose): by subject and time, without the owner, which may be
-- null and so is matched as a filter.
CREATE INDEX IF NOT EXISTS turns_subject_started ON floor.turns (subject, started_at DESC);
