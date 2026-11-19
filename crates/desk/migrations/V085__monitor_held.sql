-- What one of the person's monitors saw, held for them by their cadence:
-- through the quiet hours, until the roundup, or while they are away from
-- the DM it is for. Kept until it is told, so a restart loses none of it.
-- Additive.
CREATE TABLE IF NOT EXISTS monitor_held (
    id           UUID        PRIMARY KEY,
    agent        TEXT        NOT NULL,
    owner        TEXT        NOT NULL,
    monitor_id   TEXT        NOT NULL,
    monitor_name TEXT        NOT NULL,
    -- `dm` or `notice`: how it is told in the agent's DM.
    place        TEXT        NOT NULL,
    text         TEXT        NOT NULL,
    -- Why it waits: `quiet`, `roundup`, `away` or `typing`.
    reason       TEXT        NOT NULL,
    held_at      TIMESTAMPTZ NOT NULL,
    -- Told no sooner than this, and only once the person is back.
    release_at   TIMESTAMPTZ NOT NULL,
    -- Taken to tell until then: a telling that fails or dies with the hub
    -- leaves it to be taken again after.
    claimed_until TIMESTAMPTZ,
    -- Tellings that failed: each puts it off longer, and past a few it is
    -- dropped, so one that can't be told holds up nothing for good.
    attempts     INTEGER     NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS monitor_held_release ON monitor_held (release_at);
-- One DM's findings: what waits for it, and what is being told there.
CREATE INDEX IF NOT EXISTS monitor_held_dm ON monitor_held (agent, owner, place, release_at);
