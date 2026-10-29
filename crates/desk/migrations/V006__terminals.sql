-- Plain login shells Desk opens. `cwd` follows the shell as it moves, and
-- `env` is what Desk asked the shell to carry, so one that a reboot ended
-- reopens where it was, as it was.
CREATE TABLE terminals (
    id         UUID        PRIMARY KEY,
    name       TEXT        NOT NULL,
    cwd        TEXT        NOT NULL,
    env        JSONB       NOT NULL DEFAULT '{}',
    shell      TEXT        NOT NULL,
    state      TEXT        NOT NULL,
    exit_code  INTEGER,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
