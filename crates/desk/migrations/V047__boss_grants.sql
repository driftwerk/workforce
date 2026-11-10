-- Grants (remote bosses, RB1): what the home (the hub on the person's Mac,
-- which keeps the one sidebar) told this engine about each bossed group. It
-- is this engine's own copy, decided from while the home is away, and on a
-- machine the only place its sessions' groups come from. Written only by
-- Desk's native side, with the hub's own token.
CREATE TABLE boss_grants (
    -- The home's id for the group; `ungrouped` for the ungrouped scope.
    group_id      TEXT        PRIMARY KEY,
    name          TEXT        NOT NULL,
    -- The home's count of its writes: only ever higher.
    revision      BIGINT      NOT NULL,
    grant_version INT         NOT NULL,
    -- `local:<agent>` | `remote:<hub>/<agent>`; null: it serves no one.
    boss          TEXT,
    level         TEXT        NOT NULL,   -- watch | suggest | act | full
    sees          TEXT        NOT NULL,   -- status | replies | everything
    recall        BOOLEAN     NOT NULL DEFAULT false,
    folders       TEXT[]      NOT NULL DEFAULT '{}',
    repos         TEXT[]      NOT NULL DEFAULT '{}',
    -- This engine's sessions in the group, as the home listed them, and
    -- those started here in it that the home hasn't listed yet.
    members       UUID[]      NOT NULL DEFAULT '{}',
    born          UUID[]      NOT NULL DEFAULT '{}',
    act           JSONB       NOT NULL DEFAULT '{}',
    -- Set once stopped; it stays stopped until the home resumes it.
    stopped_at    TIMESTAMPTZ,
    stopped_from  TEXT,                   -- desk | machine
    issued_at     TIMESTAMPTZ NOT NULL,
    expires_at    TIMESTAMPTZ NOT NULL
);

-- Each session whose rows were restamped from a machine's old layout to its
-- grant's group: once, the first time it was in a grant, and never again.
CREATE TABLE boss_restamped (
    session  UUID        PRIMARY KEY,
    group_id TEXT        NOT NULL,
    at       TIMESTAMPTZ NOT NULL
);

-- The engine's own switches for bosses: one row.
CREATE TABLE boss_engine (
    one          BOOLEAN     PRIMARY KEY DEFAULT true CHECK (one),
    -- Every boss stopped at once (Stop all remote bosses, or a stop on the
    -- machine); cleared only by Desk on the Mac.
    paused       BOOLEAN     NOT NULL DEFAULT false,
    paused_at    TIMESTAMPTZ,
    paused_from  TEXT,
    -- When a machine began taking its sessions' groups from its grants, and
    -- each session's group in its own old layout then (item to group id),
    -- which the first restamp of each session reads.
    grants_since TIMESTAMPTZ,
    old_groups   JSONB
);
INSERT INTO boss_engine (one) VALUES (true);
