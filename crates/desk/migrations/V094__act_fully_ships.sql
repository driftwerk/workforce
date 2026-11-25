-- What a boss's review session said of a pull request, at the head it
-- reviewed (DW-761). `boss`: the boss agent whose review session it was;
-- a merge reads only its own boss's reviews. `owner`/`repo`: the pull
-- request's on GitHub, in lowercase. A merge goes on only on a clean one
-- at the pull request's head now; recorded again at the same head by the
-- same boss, the verdict is updated. Additive.
CREATE TABLE IF NOT EXISTS pr_reviews (
    boss       TEXT        NOT NULL,
    owner      TEXT        NOT NULL,
    repo       TEXT        NOT NULL,
    number     BIGINT      NOT NULL,
    head_sha   TEXT        NOT NULL,
    clean      BOOLEAN     NOT NULL,
    by_session UUID        NOT NULL,
    at         TIMESTAMPTZ NOT NULL,
    UNIQUE (boss, owner, repo, number, head_sha)
);

-- The pull request each review session was started to review, and by
-- which boss: a review is recorded from this link, never from what a boss
-- says it reviewed.
CREATE TABLE IF NOT EXISTS pr_review_sessions (
    session_id UUID   PRIMARY KEY,
    boss       TEXT   NOT NULL,
    owner      TEXT   NOT NULL,
    repo       TEXT   NOT NULL,
    number     BIGINT NOT NULL
);

-- A session a boss holds at Act fully, on one of the person's directives:
-- kept so a hub restart never drops it. `question`: the open item that
-- asks the person; `until`: `person_answers`, `directive_withdrawn`, or
-- `condition` with the boss's words in `condition`.
CREATE TABLE IF NOT EXISTS boss_holds (
    session_id   UUID PRIMARY KEY,
    boss         TEXT NOT NULL,
    owner        TEXT NOT NULL,
    directive_id UUID NOT NULL,
    question     UUID NOT NULL,
    until        TEXT NOT NULL,
    condition    TEXT
);
