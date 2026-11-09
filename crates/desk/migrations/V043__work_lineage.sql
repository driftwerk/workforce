-- Handoff (session memory, SM6): a session continued in another engine is
-- one piece of work with the session it came from. One row per session
-- that was handed work, naming the work's first session (`work`), the one
-- it came from, the brief as given and what it held. The first session of
-- a work has no row; a session handed back to gets one. `group_id` is the
-- sidebar group the work was in when it was handed over (null:
-- ungrouped): its brief is read only from there.
CREATE TABLE work_lineage (
    session      UUID        PRIMARY KEY,
    work         UUID        NOT NULL,
    from_session UUID        NOT NULL,
    handed_at    TIMESTAMPTZ NOT NULL,
    engine       TEXT        NOT NULL,
    account      TEXT,
    group_id     TEXT,
    brief        TEXT,
    included     JSONB       NOT NULL
);
CREATE INDEX work_lineage_work ON work_lineage (work);
CREATE INDEX work_lineage_from ON work_lineage (from_session);
-- The session a handed-off session's work continues in, while it does.
ALTER TABLE sessions ADD COLUMN handed_to UUID;
