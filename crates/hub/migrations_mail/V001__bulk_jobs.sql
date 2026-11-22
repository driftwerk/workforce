CREATE SCHEMA IF NOT EXISTS mail;
CREATE TABLE IF NOT EXISTS mail.bulk_jobs (
    id         TEXT        PRIMARY KEY,
    owner      TEXT        NOT NULL,
    account    TEXT        NOT NULL,
    job        JSONB       NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS bulk_jobs_owner_account
    ON mail.bulk_jobs (owner, account, created_at DESC);
