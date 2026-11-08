-- Moves between sidebar groups (boss-gaps W8): what a move was previewed
-- to do, the handover notes it carries, and when each session's group
-- changed.
--
-- `moves` is a preview the person confirms within ten minutes: the item
-- (`session:<id>` | `ticket:<id>` | `room:<id>` | `boss:<agent>`), where
-- it goes (`to_group`, null: ungrouped), what the person chose, and the
-- effects shown, which must still be the effects when it is confirmed.
CREATE TABLE moves (
    id           UUID        PRIMARY KEY,
    item         TEXT        NOT NULL,
    from_group   TEXT,
    to_group     TEXT,
    choices      JSONB       NOT NULL,
    effects      JSONB       NOT NULL,
    state        TEXT        NOT NULL,   -- previewed | done | dropped
    expires_at   TIMESTAMPTZ NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL,
    done_at      TIMESTAMPTZ
);
-- A note from the boss an item leaves to the boss it goes to: drafted by
-- the old boss (or written by Desk from the facts), read and perhaps
-- edited by the person, and delivered only when the move is made. It is
-- the only thing about the item that crosses from one group to the other.
-- `item` is what it is about (`plan:<id>` too, for a boss's plans);
-- `drafted_by` the old boss or Desk, as a party; `asked_until` how long
-- the old boss has to draft it.
CREATE TABLE handovers (
    id           UUID        PRIMARY KEY,
    move         UUID        NOT NULL REFERENCES moves (id) ON DELETE CASCADE,
    item         TEXT        NOT NULL,
    from_group   TEXT,
    to_group     TEXT,
    from_boss    TEXT,
    to_boss      TEXT,
    note         TEXT        NOT NULL,
    drafted_by   JSONB       NOT NULL,
    asked_until  TIMESTAMPTZ,
    edited       BOOLEAN     NOT NULL DEFAULT false,
    state        TEXT        NOT NULL,   -- draft | delivered | discarded
    created_at   TIMESTAMPTZ NOT NULL,
    delivered_at TIMESTAMPTZ
);
CREATE INDEX handovers_move ON handovers (move);
-- Each time a session's sidebar group changed. What is read about a
-- session after it moved (its ledger rows, its exchanges), but happened
-- before, is stamped with the group it happened in: the first move after
-- it, moved from.
CREATE TABLE group_moves (
    item       TEXT        NOT NULL,
    from_group TEXT,
    to_group   TEXT,
    at         TIMESTAMPTZ NOT NULL
);
CREATE INDEX group_moves_item ON group_moves (item, at);
