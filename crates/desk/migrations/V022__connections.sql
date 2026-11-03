-- What each connection to a service last said when it was checked: one row
-- per connector, level (`all`: all my agents; `agent`: just that agent),
-- agent ('' for all my agents) and person. The keys themselves are only in
-- the vault; nothing here is a secret. `detail` is the service's own words,
-- with secrets tokenised.
CREATE TABLE connections (
    connector    TEXT        NOT NULL,
    level        TEXT        NOT NULL,   -- all | agent
    agent        TEXT        NOT NULL DEFAULT '',
    person       TEXT        NOT NULL,
    account      TEXT,
    status       TEXT        NOT NULL,   -- connected | needs_reconnect | failing
    detail       TEXT,
    connected_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    checked_at   TIMESTAMPTZ,
    PRIMARY KEY (connector, level, agent, person)
);
