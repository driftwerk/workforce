-- What a boss's review session said of a pull request, at the head it
-- reviewed (DW-761). `owner`/`repo`: the pull request's on GitHub. A merge
-- goes on only on a clean one at the pull request's head now; recorded
-- again at the same head, the verdict is updated. Additive.
CREATE TABLE IF NOT EXISTS pr_reviews (
    owner      TEXT        NOT NULL,
    repo       TEXT        NOT NULL,
    number     BIGINT      NOT NULL,
    head_sha   TEXT        NOT NULL,
    clean      BOOLEAN     NOT NULL,
    by_session UUID        NOT NULL,
    at         TIMESTAMPTZ NOT NULL,
    UNIQUE (owner, repo, number, head_sha)
);

-- The pull request each review session was started to review: a review is
-- recorded from this link, never from what a boss says it reviewed.
CREATE TABLE IF NOT EXISTS pr_review_sessions (
    session_id UUID   PRIMARY KEY,
    owner      TEXT   NOT NULL,
    repo       TEXT   NOT NULL,
    number     BIGINT NOT NULL
);
