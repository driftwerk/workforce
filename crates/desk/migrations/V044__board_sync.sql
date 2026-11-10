-- Boards synced with Linear or Jira (BH10). A board's link is `boards.sync`
-- and each synced ticket's issue `tickets.external` (both from V029).
--
-- `sync_hubs`: the hubs a board can sync through, as the person linked
-- them: a name and the hub's URL. The person's token for each is in this
-- hub's vault only (`users/<person>/desk/hubs/<id>/remote_mcp`), never here.
CREATE TABLE sync_hubs (
    id         TEXT        PRIMARY KEY,
    name       TEXT        NOT NULL,
    url        TEXT        NOT NULL,
    person     TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

-- The push outbox: a synced ticket changed here and its issue is yet to
-- hear of it. One row per ticket; what goes out is worked out when it is
-- sent, from the ticket and what its issue last said. `creating_at`: a
-- create was sent and its answer not yet kept, so it is looked for before
-- it is sent again. `waiting`: the hub holds a call for the person's
-- approval, and it isn't sent again until `due_at`.
CREATE TABLE board_outbox (
    ticket      UUID        PRIMARY KEY REFERENCES tickets (id) ON DELETE CASCADE,
    board       UUID        NOT NULL REFERENCES boards (id) ON DELETE CASCADE,
    due_at      TIMESTAMPTZ NOT NULL,
    tries       INTEGER     NOT NULL DEFAULT 0,
    creating_at TIMESTAMPTZ,
    waiting     TEXT,
    error       TEXT,
    queued_at   TIMESTAMPTZ NOT NULL
);
CREATE INDEX board_outbox_due ON board_outbox (board, due_at);
