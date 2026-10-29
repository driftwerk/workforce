-- What the person calls things in Desk: a session's own name, and the
-- sidebar's groups of sessions and agents (`session:<id>`,
-- `agent:<hub>/<agent>`), in order. `ungrouped` is the fixed group of
-- everything in no other; it always comes last.
ALTER TABLE sessions ADD COLUMN display_name TEXT;

CREATE TABLE groups (
    id         TEXT        PRIMARY KEY,
    name       TEXT        NOT NULL,
    position   INTEGER     NOT NULL,
    collapsed  BOOLEAN     NOT NULL DEFAULT false,
    updated_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE group_members (
    item     TEXT    PRIMARY KEY,
    group_id TEXT    NOT NULL REFERENCES groups (id) ON DELETE CASCADE,
    position INTEGER NOT NULL
);

CREATE INDEX group_members_by_group ON group_members (group_id, position);
