-- CLI sessions the person removed from Desk: adoption never takes one again
-- (it may still run in another terminal) until they start tracking it again.
-- What it was called and where it ran are kept to list it by.
CREATE TABLE hidden_sessions (
    engine         TEXT        NOT NULL,
    cli_session_id TEXT        NOT NULL,
    name           TEXT        NOT NULL,
    cwd            TEXT        NOT NULL,
    hidden_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (engine, cli_session_id)
);
