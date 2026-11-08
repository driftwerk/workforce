-- Plans: a boss's goal broken into tickets on a board, which the person
-- approves before any of it runs. Order and parallelism come from the
-- tickets' needs (`ticket_needs`), so waves are computed, never stored.
--
-- `state`: 'proposed' | 'running' | 'paused' | 'done' | 'dropped'.
-- `paused`: why a running plan starts nothing ('person' | 'budget' |
-- 'boss_held' | 'restoring' | 'no_boss'); null while it isn't paused.
-- `parallel`: this plan's limit of sessions at once, when lower than its
-- boss's; null is the boss's. `revision` is the newest revision;
-- `approved_revision` the one the runner follows (none before the person
-- approves). `group_id` is the board's group when the plan was made, with
-- no foreign key, as for boards.
CREATE TABLE plans (
    id                UUID        PRIMARY KEY,
    board             UUID        NOT NULL REFERENCES boards (id) ON DELETE CASCADE,
    group_id          TEXT,
    boss              TEXT        NOT NULL,
    goal              TEXT        NOT NULL,
    state             TEXT        NOT NULL,
    paused            TEXT,
    parallel          SMALLINT,
    revision          INTEGER     NOT NULL DEFAULT 1,
    approved_revision INTEGER,
    created_at        TIMESTAMPTZ NOT NULL,
    updated_at        TIMESTAMPTZ NOT NULL
);
CREATE INDEX plans_state ON plans (state);
-- Each revision of a plan, kept: `snapshot` is its goal and tickets (title,
-- body, engine, folder, view, needs) as of it; `change` is 'minor' or
-- 'scope'; `made_by` the boss or the person, as a party.
CREATE TABLE plan_revisions (
    plan     UUID        NOT NULL REFERENCES plans (id) ON DELETE CASCADE,
    revision INTEGER     NOT NULL,
    at       TIMESTAMPTZ NOT NULL,
    made_by  JSONB       NOT NULL,
    change   TEXT        NOT NULL,
    summary  TEXT        NOT NULL,
    snapshot JSONB       NOT NULL,
    PRIMARY KEY (plan, revision)
);
-- A plan's tickets are the board's own, with what their sessions start in.
ALTER TABLE tickets ADD COLUMN plan UUID REFERENCES plans (id) ON DELETE SET NULL,
    ADD COLUMN engine TEXT,
    ADD COLUMN folder TEXT,
    ADD COLUMN view TEXT;
CREATE INDEX tickets_plan ON tickets (plan, state) WHERE plan IS NOT NULL;
