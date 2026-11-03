-- The person's accounts per engine (several subscription logins, "Work" and
-- "Personal" say), at most one of them each engine's default, and which one
-- each session runs on. An account's sign-in is kept in Desk's secret store,
-- never here. A session with no account runs on the CLI's own sign-in on
-- this computer, as every session did before.
CREATE TABLE engine_accounts (
    id         UUID        PRIMARY KEY,
    engine     TEXT        NOT NULL,
    name       TEXT        NOT NULL,
    is_default BOOLEAN     NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE UNIQUE INDEX engine_accounts_name ON engine_accounts (engine, lower(name));
CREATE UNIQUE INDEX engine_accounts_default ON engine_accounts (engine) WHERE is_default;
ALTER TABLE sessions ADD COLUMN account UUID;
