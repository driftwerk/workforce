CREATE TABLE sessions (
    id             UUID PRIMARY KEY,
    engine         TEXT        NOT NULL,
    name           TEXT        NOT NULL,
    goal           TEXT        NOT NULL DEFAULT '',
    cwd            TEXT        NOT NULL,
    branch         TEXT,
    origin         TEXT        NOT NULL,
    state          TEXT        NOT NULL,
    approvals      TEXT        NOT NULL,
    cli_session_id TEXT,
    transcript     TEXT,
    last_prompt    TEXT,
    last_reply     TEXT,
    external_pid   INTEGER,
    restore_offer  BOOLEAN     NOT NULL DEFAULT FALSE,
    created_at     TIMESTAMPTZ NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL
);

CREATE UNIQUE INDEX sessions_cli_session ON sessions (engine, cli_session_id) WHERE cli_session_id IS NOT NULL;
CREATE INDEX sessions_updated ON sessions (updated_at DESC);
