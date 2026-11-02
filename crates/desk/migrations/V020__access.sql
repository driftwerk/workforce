-- What agents may touch on this computer: the kill switch, the grants a
-- person gave ("always allow in this folder", "for this session"), and
-- every call the verdict decided. `person` and `arguments` are personal,
-- erased with the person; `arguments` is kept with secrets tokenised.
CREATE TABLE access_settings (
    id         BOOLEAN     PRIMARY KEY DEFAULT true CHECK (id),
    stopped    BOOLEAN     NOT NULL DEFAULT false,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE access_grants (
    id         UUID        PRIMARY KEY,
    hub        TEXT        NOT NULL DEFAULT 'local',
    agent      TEXT        NOT NULL,
    person     TEXT        NOT NULL,
    ability    TEXT        NOT NULL,   -- write | delete | run | session | connector_write
    folder     TEXT,
    connector  TEXT,
    -- Set for an allowance that lasts this session only.
    session_id TEXT,
    expires_at TIMESTAMPTZ,
    -- Set for a grant held by one job or workflow.
    run_kind   TEXT,
    run_id     TEXT,
    run_name   TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX access_grants_agent ON access_grants (hub, agent, ability);
CREATE TABLE access_activity (
    id           UUID        PRIMARY KEY,
    hub          TEXT        NOT NULL DEFAULT 'local',
    agent        TEXT        NOT NULL,
    person       TEXT,
    session_id   TEXT,
    request_id   TEXT,
    kind         TEXT        NOT NULL,   -- read | write | delete | exec | session | connector
    doing        TEXT        NOT NULL,
    folder       TEXT,
    connector    TEXT,
    tool         TEXT        NOT NULL,
    vault_source TEXT,
    decided_by   TEXT        NOT NULL,   -- rule | grant | session | person | classifier
    outcome      JSONB       NOT NULL,
    reason       TEXT,
    exit_code    INT,
    duration_ms  INT,
    arguments    JSONB       NOT NULL,
    at           TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX access_activity_agent_at ON access_activity (agent, at DESC);
CREATE INDEX access_activity_at ON access_activity (at DESC);
