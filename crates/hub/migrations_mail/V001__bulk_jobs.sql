CREATE SCHEMA IF NOT EXISTS mail;
CREATE TABLE IF NOT EXISTS mail.bulk_jobs (
    id         TEXT        PRIMARY KEY,
    owner      TEXT        NOT NULL,
    account    TEXT        NOT NULL,
    origin     TEXT        NOT NULL,
    state      TEXT        NOT NULL,
    job        JSONB       NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS bulk_jobs_owner_account
    ON mail.bulk_jobs (owner, account, created_at DESC);
CREATE INDEX IF NOT EXISTS bulk_jobs_updated_at
    ON mail.bulk_jobs (updated_at);
CREATE TABLE IF NOT EXISTS mail.bulk_batches (
    job_id TEXT    NOT NULL REFERENCES mail.bulk_jobs (id) ON DELETE CASCADE,
    seq    INTEGER NOT NULL,
    batch  JSONB   NOT NULL,
    PRIMARY KEY (job_id, seq)
);
