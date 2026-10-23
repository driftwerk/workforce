-- Telemetry store schema (Postgres backend, Phase 1B-2).
--
-- Translation of `crates/common/src/persistence.rs::SqliteStore::migrate`,
-- consolidated to the final post-ALTER shape so a fresh deployment gets
-- the complete schema in one migration. Subsequent schema changes ship
-- as `V00n__*.sql` files alongside this one.
--
-- All tables live in the schema named when this set is registered
-- (`telemetry` by default — see `MigrationSet` in
-- `crates/common/src/postgres/migrations.rs`). The migration runner sets
-- `search_path` before applying.
--
-- Type translations from SQLite:
--   INTEGER PRIMARY KEY AUTOINCREMENT  →  BIGSERIAL PRIMARY KEY
--   INTEGER NOT NULL DEFAULT 0         →  BIGINT NOT NULL DEFAULT 0
--   INTEGER NOT NULL DEFAULT 1 (bool)  →  BOOLEAN NOT NULL DEFAULT TRUE
--   REAL NOT NULL                      →  DOUBLE PRECISION NOT NULL
--   TEXT (RFC3339 timestamp)           →  TIMESTAMPTZ
--   TEXT NOT NULL                      →  TEXT NOT NULL
--   TEXT (nullable)                    →  TEXT (nullable)

CREATE TABLE IF NOT EXISTS tasks (
    id              TEXT PRIMARY KEY,
    title           TEXT NOT NULL,
    description     TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'created',
    priority        TEXT NOT NULL DEFAULT 'normal',
    assignee        TEXT,
    team_id         TEXT,
    labels          TEXT NOT NULL DEFAULT '[]',
    parent_id       TEXT,
    result_json     TEXT,
    created_at      TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL,
    started_at      TIMESTAMPTZ,
    completed_at    TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
CREATE INDEX IF NOT EXISTS idx_tasks_team ON tasks(team_id);

CREATE TABLE IF NOT EXISTS audit_log (
    id          BIGSERIAL PRIMARY KEY,
    timestamp   TIMESTAMPTZ NOT NULL,
    actor       TEXT NOT NULL,
    action      TEXT NOT NULL,
    resource    TEXT NOT NULL,
    detail      TEXT,
    outcome     TEXT NOT NULL DEFAULT 'success'
);
CREATE INDEX IF NOT EXISTS idx_audit_timestamp ON audit_log(timestamp);
CREATE INDEX IF NOT EXISTS idx_audit_actor ON audit_log(actor);

CREATE TABLE IF NOT EXISTS cost_records (
    id              BIGSERIAL PRIMARY KEY,
    timestamp       TIMESTAMPTZ NOT NULL,
    agent_id        TEXT NOT NULL,
    model           TEXT NOT NULL,
    input_tokens    BIGINT NOT NULL,
    output_tokens   BIGINT NOT NULL,
    cost_usd        DOUBLE PRECISION NOT NULL,
    team_id         TEXT,
    task_id         TEXT,
    task_type       TEXT,
    ticket_id       TEXT
);
CREATE INDEX IF NOT EXISTS idx_cost_timestamp ON cost_records(timestamp);
CREATE INDEX IF NOT EXISTS idx_cost_agent ON cost_records(agent_id);
CREATE INDEX IF NOT EXISTS idx_cost_records_ticket ON cost_records(ticket_id);

CREATE TABLE IF NOT EXISTS task_telemetry (
    id                              BIGSERIAL PRIMARY KEY,
    agent_id                        TEXT NOT NULL,
    model                           TEXT NOT NULL,
    provider                        TEXT NOT NULL DEFAULT 'anthropic',
    input_tokens                    BIGINT NOT NULL,
    output_tokens                   BIGINT NOT NULL,
    cache_creation_input_tokens     BIGINT NOT NULL DEFAULT 0,
    cache_read_input_tokens         BIGINT NOT NULL DEFAULT 0,
    cost_usd                        DOUBLE PRECISION NOT NULL,
    rounds                          BIGINT NOT NULL,
    duration_ms                     BIGINT NOT NULL,
    tool_invocation_count           BIGINT NOT NULL DEFAULT 0,
    success                         BOOLEAN NOT NULL DEFAULT TRUE,
    error_type                      TEXT,
    retry_count                     BIGINT NOT NULL DEFAULT 0,
    task_type                       TEXT,
    task_id                         TEXT,
    started_at                      TIMESTAMPTZ NOT NULL,
    completed_at                    TIMESTAMPTZ NOT NULL,
    forecast_cost_usd               DOUBLE PRECISION,
    forecast_rounds                 BIGINT,
    forecast_confidence             DOUBLE PRECISION,
    team_id                         TEXT,
    session_id                      TEXT,
    model_switches                  TEXT,
    ticket_id                       TEXT
);
CREATE INDEX IF NOT EXISTS idx_task_telemetry_agent ON task_telemetry(agent_id);
CREATE INDEX IF NOT EXISTS idx_task_telemetry_model ON task_telemetry(model);
CREATE INDEX IF NOT EXISTS idx_task_telemetry_completed ON task_telemetry(completed_at);
CREATE INDEX IF NOT EXISTS idx_task_telemetry_task_type ON task_telemetry(task_type);
CREATE INDEX IF NOT EXISTS idx_task_telemetry_agent_model ON task_telemetry(agent_id, model);
CREATE INDEX IF NOT EXISTS idx_task_telemetry_team ON task_telemetry(team_id);
CREATE INDEX IF NOT EXISTS idx_task_telemetry_session ON task_telemetry(session_id);
CREATE INDEX IF NOT EXISTS idx_task_telemetry_ticket ON task_telemetry(ticket_id);

CREATE TABLE IF NOT EXISTS round_telemetry (
    id                      BIGSERIAL PRIMARY KEY,
    task_telemetry_id       BIGINT NOT NULL REFERENCES task_telemetry(id) ON DELETE CASCADE,
    round_number            BIGINT NOT NULL,
    input_tokens            BIGINT NOT NULL,
    output_tokens           BIGINT NOT NULL,
    latency_ms              BIGINT NOT NULL,
    cache_creation_tokens   BIGINT NOT NULL DEFAULT 0,
    cache_read_tokens       BIGINT NOT NULL DEFAULT 0,
    prompt_size_bytes       BIGINT NOT NULL DEFAULT 0,
    response_size_bytes     BIGINT NOT NULL DEFAULT 0,
    tool_count              BIGINT NOT NULL DEFAULT 0,
    stop_reason             TEXT NOT NULL DEFAULT 'end_turn'
);
CREATE INDEX IF NOT EXISTS idx_round_telemetry_task ON round_telemetry(task_telemetry_id);

CREATE TABLE IF NOT EXISTS tool_telemetry (
    id                      BIGSERIAL PRIMARY KEY,
    task_telemetry_id       BIGINT NOT NULL REFERENCES task_telemetry(id) ON DELETE CASCADE,
    round_number            BIGINT NOT NULL,
    tool_name               TEXT NOT NULL,
    success                 BOOLEAN NOT NULL DEFAULT TRUE,
    duration_ms             BIGINT NOT NULL DEFAULT 0,
    policy_decision         TEXT NOT NULL DEFAULT 'allow',
    input_size_bytes        BIGINT NOT NULL DEFAULT 0,
    output_size_bytes       BIGINT NOT NULL DEFAULT 0,
    denial_reason           TEXT
);
CREATE INDEX IF NOT EXISTS idx_tool_telemetry_task ON tool_telemetry(task_telemetry_id);

CREATE TABLE IF NOT EXISTS model_health_events (
    id          BIGSERIAL PRIMARY KEY,
    timestamp   TIMESTAMPTZ NOT NULL,
    model       TEXT NOT NULL,
    event_type  TEXT NOT NULL,
    detail      TEXT,
    agent_id    TEXT
);
CREATE INDEX IF NOT EXISTS idx_model_health_model ON model_health_events(model);
CREATE INDEX IF NOT EXISTS idx_model_health_timestamp ON model_health_events(timestamp);

CREATE TABLE IF NOT EXISTS agent_model_preferences (
    agent_id        TEXT NOT NULL,
    task_type       TEXT NOT NULL,
    preferred_model TEXT NOT NULL,
    reason          TEXT,
    updated_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (agent_id, task_type)
);

CREATE TABLE IF NOT EXISTS policy_proposals (
    id              BIGSERIAL PRIMARY KEY,
    agent_id        TEXT NOT NULL,
    proposed_rule   TEXT NOT NULL,
    justification   TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending',
    reviewed_by     TEXT,
    review_note     TEXT,
    created_at      TIMESTAMPTZ NOT NULL,
    reviewed_at     TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_proposals_status ON policy_proposals(status);
CREATE INDEX IF NOT EXISTS idx_proposals_agent ON policy_proposals(agent_id);

CREATE TABLE IF NOT EXISTS model_pricing (
    model               TEXT PRIMARY KEY,
    provider            TEXT NOT NULL,
    input_per_million   DOUBLE PRECISION NOT NULL,
    output_per_million  DOUBLE PRECISION NOT NULL,
    source              TEXT NOT NULL DEFAULT 'default',
    fetched_at          TIMESTAMPTZ NOT NULL,
    updated_at          TIMESTAMPTZ NOT NULL
);

CREATE TABLE IF NOT EXISTS workflow_instances (
    id                  TEXT PRIMARY KEY,
    workflow_id         TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'running',
    current_step_id     TEXT,
    step_history        TEXT NOT NULL DEFAULT '[]',
    variables           TEXT NOT NULL DEFAULT '{}',
    trigger_json        TEXT NOT NULL,
    agent_id            TEXT,
    created_at          TIMESTAMPTZ NOT NULL,
    updated_at          TIMESTAMPTZ NOT NULL,
    parent_instance_id  TEXT,
    channel_id          TEXT,
    thread_id           TEXT
);
CREATE INDEX IF NOT EXISTS idx_workflow_instances_status ON workflow_instances(status);
CREATE INDEX IF NOT EXISTS idx_workflow_instances_workflow ON workflow_instances(workflow_id);
CREATE INDEX IF NOT EXISTS idx_workflow_instances_parent ON workflow_instances(parent_instance_id);
CREATE INDEX IF NOT EXISTS idx_workflow_instances_thread ON workflow_instances(channel_id, thread_id);

CREATE TABLE IF NOT EXISTS security_events (
    id              BIGSERIAL PRIMARY KEY,
    timestamp       TIMESTAMPTZ NOT NULL,
    event_type      TEXT NOT NULL,
    agent_id        TEXT NOT NULL,
    session_id      TEXT,
    severity        TEXT NOT NULL,
    source          TEXT,
    detail          TEXT NOT NULL,
    content_hash    TEXT
);
CREATE INDEX IF NOT EXISTS idx_security_events_timestamp ON security_events(timestamp);
CREATE INDEX IF NOT EXISTS idx_security_events_type ON security_events(event_type);
CREATE INDEX IF NOT EXISTS idx_security_events_agent ON security_events(agent_id);
CREATE INDEX IF NOT EXISTS idx_security_events_severity ON security_events(severity);
