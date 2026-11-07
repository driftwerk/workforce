-- The person's switches, by key (`memory.paused`, `memory.group.<id>`):
-- what they set in Settings, beside the install's defaults in the hub's
-- config.
CREATE TABLE settings (
    key        TEXT        PRIMARY KEY,
    value      JSONB       NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
