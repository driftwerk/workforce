-- Session memory's notes (SM3): when each session's exchanges were last
-- read for decisions, facts and to-dos, so a session is read at most every
-- ten minutes as its turns end.
ALTER TABLE memory_derive ADD COLUMN derived_at TIMESTAMPTZ;

-- What the derivation worker claims next: rows due, soonest first.
CREATE INDEX memory_derive_due ON memory_derive (due_at) WHERE due_at IS NOT NULL;

-- How many calls the model made to note decisions each day, in the
-- person's time, against `derive_calls_per_day`.
CREATE TABLE memory_derive_days (
    day   DATE PRIMARY KEY,
    calls INT  NOT NULL DEFAULT 0
);

-- How far each session's exchanges in each group were read for notes: a
-- group whose sessions aren't read yet holds only its own place, so the
-- session's later work elsewhere is still noted. `group_key` is the
-- group's id, or '' for ungrouped work.
CREATE TABLE memory_derive_reads (
    session    UUID NOT NULL,
    group_key  TEXT NOT NULL,
    derived_to INT  NOT NULL DEFAULT 0,
    PRIMARY KEY (session, group_key)
);
