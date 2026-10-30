-- Which nodes are online, and whose, for turns on every hub instance:
-- one row per node key, put by the hub instance holding the node's
-- connection when it registers and every 30 s after. A row is online while
-- `online` and seen in the last 90 s, so a crashed instance's rows lapse on
-- their own; `seen_at` of a row gone offline is when it was last seen.
--
-- A new table and its index: no lock on anything that exists.

CREATE TABLE IF NOT EXISTS presence (
    key         TEXT PRIMARY KEY,
    identity_id TEXT NOT NULL,
    kind        JSONB NOT NULL,
    instance_id TEXT NOT NULL,
    online      BOOLEAN NOT NULL DEFAULT true,
    seen_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS presence_identity ON presence (identity_id, seen_at DESC);
