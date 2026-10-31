-- How far a person has read their Desk conversation with an agent, so every
-- Desk of theirs agrees. Rows are the person's; the transcript stays the
-- session's. A row with an empty agent and channel is the person's floor:
-- when this hub began keeping their points, before which nothing counts as
-- unread. Additive, and tenant-first as V011 asks, so a replica on the
-- previous binary is unaffected and the edge can share the set.
CREATE TABLE IF NOT EXISTS desk_reads (
    tenant_id  TEXT        NOT NULL DEFAULT 'default' CHECK (tenant_id <> ''),
    owner      TEXT        NOT NULL,
    agent_id   TEXT        NOT NULL,
    channel    TEXT        NOT NULL,
    at         TIMESTAMPTZ NOT NULL,
    message_id TEXT,
    updated_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (tenant_id, owner, agent_id, channel)
);
