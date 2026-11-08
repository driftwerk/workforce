-- Subjects the person had Desk forget, and when: a gist of a turn that
-- began before then is never written back, though a job for it was
-- already under way.
CREATE TABLE floor.gists_forgotten (
    subject TEXT        PRIMARY KEY,
    at      TIMESTAMPTZ NOT NULL
);
