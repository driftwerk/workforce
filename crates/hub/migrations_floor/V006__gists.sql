-- What each agent or Desk session is for and what it is doing, in plain
-- words: one row per owner and subject, rewritten as its turns run.
CREATE TABLE floor.gists (
    owner      TEXT,
    subject    TEXT NOT NULL,
    purpose    TEXT,
    purpose_at TIMESTAMPTZ,
    now_text   TEXT NOT NULL,
    now_at     TIMESTAMPTZ NOT NULL,
    turn       UUID NOT NULL,
    by_model   BOOLEAN NOT NULL
);

CREATE UNIQUE INDEX gists_subject ON floor.gists ((coalesce(owner, '')), subject);

-- The floor in one paragraph, for each person, and for their personal
-- hub's own sessions with it.
CREATE TABLE floor.summaries (
    owner       TEXT NOT NULL,
    personal    BOOLEAN NOT NULL,
    paragraph   TEXT NOT NULL,
    written_at  TIMESTAMPTZ NOT NULL,
    fingerprint BIGINT NOT NULL,
    PRIMARY KEY (owner, personal)
);

-- A subject's asks, read in order for what it is for.
CREATE INDEX turns_subject_started ON floor.turns (owner, subject, started_at);
