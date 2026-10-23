-- Liveness roster for hub processes. Each instance upserts its row on a short
-- interval; a row gone stale marks the owner as dead, letting turn-claim
-- acquisition steal that instance's claims immediately instead of waiting out
-- the claim TTL. Rows are never deleted by their owner (a present-but-stale
-- row IS the death signal); ancient rows are pruned opportunistically.
CREATE TABLE instance_heartbeats (
    instance_id TEXT PRIMARY KEY,
    last_seen TIMESTAMPTZ NOT NULL
);
