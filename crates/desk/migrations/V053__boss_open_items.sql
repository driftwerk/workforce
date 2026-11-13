-- What the boss owes the person (the boss understands sessions, H5): each
-- question it asked, each decision it wants confirmed, and (from H6) each
-- promise and order, until it is answered or no longer applies. One row
-- per matter, kept across restarts and DM rollovers: every way an item
-- opens goes through one insert keyed by dedupe_key, unique among the
-- boss's items not closed.
CREATE TABLE boss_open_items (
    id             UUID        PRIMARY KEY,
    -- The boss's agent id.
    boss           TEXT        NOT NULL,
    owner          TEXT        NOT NULL DEFAULT '',
    -- ask | confirm | promise | order
    kind           TEXT        NOT NULL,
    session        UUID,
    -- A session's question given to the person, when it is one.
    line_message   UUID,
    text           TEXT        NOT NULL,
    holding_up     TEXT,
    recommendation TEXT,
    -- The answers it offers.
    options        JSONB       NOT NULL DEFAULT '[]',
    -- The kind of case an answer would settle from then on.
    generalizes    TEXT,
    -- A promise's or order's end: "merged", "in production".
    done_when      TEXT,
    dedupe_key     TEXT        NOT NULL,
    opened_at      TIMESTAMPTZ NOT NULL,
    -- Last put to the person; said_count is how many times. An item never
    -- said (0) is only the watcher's mark that it told the boss of it.
    last_said_at   TIMESTAMPTZ,
    said_count     INT         NOT NULL DEFAULT 0,
    -- Last put to the boss in a check-in, so a restart doesn't wake it
    -- again for what it was told (this was hub.crew.raised).
    raised_at      TIMESTAMPTZ,
    changed_at     TIMESTAMPTZ NOT NULL,
    -- open | parked | closed
    state          TEXT        NOT NULL DEFAULT 'open',
    closed_at      TIMESTAMPTZ,
    -- person | session | boss | superseded | lapsed
    closed_by      TEXT,
    closed_why     TEXT
);
CREATE UNIQUE INDEX boss_open_items_key ON boss_open_items (boss, dedupe_key)
    WHERE state <> 'closed';
CREATE INDEX boss_open_items_boss ON boss_open_items (boss, state);
CREATE INDEX boss_open_items_line ON boss_open_items (line_message)
    WHERE line_message IS NOT NULL AND state <> 'closed';
