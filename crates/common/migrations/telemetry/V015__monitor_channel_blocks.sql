-- Channel monitors whose watching agent's bot cannot read the channel it is
-- bound to (not a member, or the channel is gone). One row per monitor while
-- the block lasts; removed by the next successful sweep of the channel or when
-- the monitor is deleted. `blocked_since` marks when the owner was told.
CREATE TABLE IF NOT EXISTS monitor_channel_blocks (
    monitor_id     TEXT NOT NULL,
    owner          TEXT NOT NULL DEFAULT '__system__',
    agent_id       TEXT NOT NULL,
    surface        TEXT NOT NULL,
    channel        TEXT NOT NULL,
    error          TEXT NOT NULL,
    blocked_since  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (monitor_id, owner)
);

CREATE INDEX IF NOT EXISTS idx_monitor_channel_blocks_channel
    ON monitor_channel_blocks (agent_id, surface, channel);
