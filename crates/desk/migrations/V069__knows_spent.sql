-- What was sent to each agent, counted by the hour (Sees): the daily byte
-- limit is read from this, so a restart of the hub no longer gives an agent
-- a fresh day. `knows_sent` can't say it: it keeps one row per item, so an
-- item sent again replaces its row. Rows older than two days are dropped as
-- new ones are written.
CREATE TABLE knows_spent (
    agent TEXT        NOT NULL,
    hour  TIMESTAMPTZ NOT NULL,
    items BIGINT      NOT NULL,
    bytes BIGINT      NOT NULL,
    PRIMARY KEY (agent, hour)
);
