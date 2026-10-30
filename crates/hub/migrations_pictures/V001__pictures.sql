-- Pictures of agents and of people: one chosen for them (in Desk or the web
-- app), and their Slack picture, which the hub fetches and refreshes. The
-- chosen one is shown when there is one, else the Slack one.
--
-- `origin` is what a Slack picture was fetched for (the identity's icon URL,
-- or the workspace and Slack user whose profile image it is): when that
-- changes, the picture is fetched again.
--
-- Two new tables: no lock on anything that exists.

CREATE TABLE IF NOT EXISTS agent_pictures (
    agent_id    TEXT NOT NULL,
    source      TEXT NOT NULL CHECK (source IN ('chosen', 'slack')),
    media_type  TEXT NOT NULL,
    bytes       BYTEA NOT NULL,
    etag        TEXT NOT NULL,
    origin      TEXT,
    workspace   TEXT,
    at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (agent_id, source)
);

CREATE TABLE IF NOT EXISTS identity_pictures (
    identity_id TEXT NOT NULL,
    source      TEXT NOT NULL CHECK (source IN ('chosen', 'slack')),
    media_type  TEXT NOT NULL,
    bytes       BYTEA NOT NULL,
    etag        TEXT NOT NULL,
    origin      TEXT,
    workspace   TEXT,
    at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (identity_id, source)
);
