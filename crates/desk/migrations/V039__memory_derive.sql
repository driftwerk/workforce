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
