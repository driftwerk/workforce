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
-- long it has been Done. `version` moves with every write: a write made
-- from an older read is refused, so two changes never overwrite each other.
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
    version       INTEGER          NOT NULL DEFAULT 0,
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
-- When each session worked on each ticket: from its assignment until it
-- was taken off it, or the ticket was Done or Dropped. What the ledger
-- stamps a session's calls by.
CREATE TABLE ticket_assignments (
    ticket   UUID        NOT NULL REFERENCES tickets (id) ON DELETE CASCADE,
    session  UUID        NOT NULL,
    from_at  TIMESTAMPTZ NOT NULL,
    until_at TIMESTAMPTZ,
    PRIMARY KEY (ticket, session, from_at)
);
CREATE INDEX ticket_assignments_session ON ticket_assignments (session, from_at);
CREATE UNIQUE INDEX ticket_assignments_open ON ticket_assignments (ticket) WHERE until_at IS NULL;
-- The activity log is the board's history: each ticket's events, and with
-- `subject`, each session's and agent's.
ALTER TABLE activity ADD COLUMN ticket UUID;
CREATE INDEX activity_ticket ON activity (ticket, at DESC) WHERE ticket IS NOT NULL;
-- The ticket a session worked on when it made the call, by
-- `ticket_assignments`.
ALTER TABLE ledger ADD COLUMN ticket UUID;
CREATE INDEX ledger_ticket ON ledger (ticket, at) WHERE ticket IS NOT NULL;
