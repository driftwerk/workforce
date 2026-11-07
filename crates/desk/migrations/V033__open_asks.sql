-- What each running session asks the person (a permission, a question, a
-- plan to approve), one row per session while it asks anything: the call
-- that raised each ask, who raised it, and its wait's key on the floor. A
-- hub that restarts reads it to show each ask again, as the terminal's to
-- answer, and to close every other wait the floor still has open.
CREATE TABLE IF NOT EXISTS open_asks (
    session UUID PRIMARY KEY,
    asks JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
