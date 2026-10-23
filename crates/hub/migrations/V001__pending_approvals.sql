-- Pending-approval store schema (Postgres backend).
--
-- Translation of `crates/hub/src/approvals/store.rs::init_schema` (SQLite).
-- All tables live under the schema named when this set is registered
-- (`approvals` by default — see `MigrationSet`). The runner sets
-- `search_path` before applying.

CREATE TABLE IF NOT EXISTS pending_approvals (
    request_id          UUID PRIMARY KEY,
    agent_id            TEXT NOT NULL,
    session_key         TEXT NOT NULL,
    workspace_id        TEXT,
    channel_id          TEXT,
    thread_ts           TEXT,
    requester_id        TEXT NOT NULL,
    tool_name           TEXT NOT NULL,
    tool_input          TEXT NOT NULL,
    reason              TEXT NOT NULL,
    allow_self_approval BOOLEAN NOT NULL,
    status              TEXT NOT NULL,
    resolved_by         TEXT,
    resolved_at         TIMESTAMPTZ,
    channel_notice      TEXT,
    channel_notice_ts   TEXT,
    created_at          TIMESTAMPTZ NOT NULL,
    expires_at          TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pending_approvals_due
    ON pending_approvals (status, expires_at);

CREATE TABLE IF NOT EXISTS pending_approval_targets (
    request_id      UUID NOT NULL REFERENCES pending_approvals (request_id) ON DELETE CASCADE,
    approver_id     TEXT NOT NULL,
    slack_user_id   TEXT NOT NULL,
    dm_channel      TEXT,
    message_ts      TEXT,
    PRIMARY KEY (request_id, approver_id)
);

CREATE INDEX IF NOT EXISTS idx_pending_approval_targets_slack
    ON pending_approval_targets (request_id, slack_user_id);
