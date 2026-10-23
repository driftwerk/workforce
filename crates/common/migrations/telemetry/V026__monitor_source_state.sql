-- What a source monitor's last poll left behind: the keys it saw (hash, a
-- small copy, whether the condition held), the cadence, and a DURABLE
-- consecutive-failure count — a failing credential must not re-earn its
-- auto-disable after every restart. `observed` is NULL until a poll has
-- succeeded, so a failed first poll is still followed by a baseline rather
-- than a flood. `next_due_at` is the poll slot: a poller takes it atomically
-- before reading, so two engines never read one source at once. Keyed like
-- the other monitor satellites, so it moves with the definition on transfer
-- and is dropped with it.
CREATE TABLE IF NOT EXISTS monitor_source_state (
    monitor_id   TEXT NOT NULL,
    owner        TEXT NOT NULL DEFAULT '__system__',
    observed     JSONB,
    polled_at    TIMESTAMPTZ,
    next_due_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    error        TEXT,
    failures     INTEGER NOT NULL DEFAULT 0,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (monitor_id, owner)
);
