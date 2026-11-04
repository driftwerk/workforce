-- The task board: boards of tickets the person owns, with dependencies,
-- links to the sessions, rooms, messages and PRs that work on them, and
-- their history in the activity log.
--
-- `group_id` is the sidebar group a board belongs to (null: ungrouped).
-- It has no foreign key: the layout is saved by deleting and inserting
-- every group, which would clear it each time. `sync` is a link to Linear
-- or Jira; a board has a prefix of its own unless it is synced.
CREATE TABLE boards (
    id          UUID        PRIMARY KEY,
    name        TEXT        NOT NULL,
    group_id    TEXT,
    prefix      TEXT        UNIQUE CHECK (prefix ~ '^[A-Z][A-Z0-9]{1,5}$'),
    next_number INTEGER     NOT NULL DEFAULT 1,
    sync        JSONB,
    CHECK (prefix IS NOT NULL OR sync IS NOT NULL),
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL
);
-- `updated_at` moves with what the ticket says and who works on it, never
-- with a pin or a snooze: a snoozed ticket wakes once it changes after
-- `snoozed_at`. `state_at` is when it came into its state, which is how
-- long it has been Done.
CREATE TABLE tickets (
    id            UUID             PRIMARY KEY,
    board         UUID             NOT NULL REFERENCES boards (id) ON DELETE CASCADE,
    number        INTEGER          NOT NULL,
    title         TEXT             NOT NULL,
    body          TEXT             NOT NULL DEFAULT '',
    state         TEXT             NOT NULL,
    rank          DOUBLE PRECISION NOT NULL,
    assignee      JSONB,
    created_by    JSONB            NOT NULL,
    external      JSONB,
    pinned        BOOLEAN          NOT NULL DEFAULT FALSE,
    snoozed_until TIMESTAMPTZ,
    snoozed_at    TIMESTAMPTZ,
    state_at      TIMESTAMPTZ      NOT NULL,
    created_at    TIMESTAMPTZ      NOT NULL,
    updated_at    TIMESTAMPTZ      NOT NULL,
    UNIQUE (board, number)
);
CREATE INDEX tickets_search ON tickets USING gin (to_tsvector('simple', title || ' ' || body));
CREATE UNIQUE INDEX tickets_external ON tickets ((external ->> 'system'), (external ->> 'key'))
    WHERE external IS NOT NULL;
CREATE INDEX tickets_column ON tickets (board, state, rank);
CREATE INDEX tickets_assignee ON tickets ((assignee ->> 'id'));
CREATE TABLE ticket_needs (
    ticket UUID NOT NULL REFERENCES tickets (id) ON DELETE CASCADE,
    needs  UUID NOT NULL REFERENCES tickets (id) ON DELETE CASCADE,
    PRIMARY KEY (ticket, needs),
    CHECK (ticket <> needs)
);
-- `kind`: 'session' | 'room' | 'message' | 'pr'; `target` the session's,
-- room's (and thread's, `room/thread`) or message's id, or the PR's number.
CREATE TABLE ticket_links (
    ticket UUID        NOT NULL REFERENCES tickets (id) ON DELETE CASCADE,
    kind   TEXT        NOT NULL,
    target TEXT        NOT NULL,
    detail JSONB,
    at     TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (ticket, kind, target)
);
CREATE INDEX ticket_links_target ON ticket_links (kind, target);
-- The activity log is the board's history: each ticket's events, and with
-- `subject`, each session's and agent's.
ALTER TABLE activity ADD COLUMN ticket UUID;
CREATE INDEX activity_ticket ON activity (ticket, at DESC) WHERE ticket IS NOT NULL;
-- The ticket a session worked on when it made the call: the one it was
-- assigned last before the call, while it still is.
ALTER TABLE ledger ADD COLUMN ticket UUID;
CREATE INDEX ledger_ticket ON ledger (ticket, at) WHERE ticket IS NOT NULL;
