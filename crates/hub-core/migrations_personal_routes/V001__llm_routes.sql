-- A person's own route: their work run on their own subscription instead of
-- the hub's model route. Two settings, the narrower winning: one for every
-- agent, and one per agent, which is either their own route there or "hub"
-- (the hub's route whatever the every-agent setting says). No row means no
-- setting. Every change is appended to `llm_route_changes`.
--
-- New tables only: no lock on anything that exists.

CREATE TABLE IF NOT EXISTS user_settings (
    identity_id  TEXT NOT NULL PRIMARY KEY,
    llm_kind     TEXT NOT NULL,
    llm_fallback TEXT NOT NULL CHECK (llm_fallback IN ('wait', 'hub')),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS user_agent_settings (
    identity_id  TEXT NOT NULL,
    agent_id     TEXT NOT NULL,
    llm_route    TEXT NOT NULL CHECK (llm_route IN ('hub', 'personal')),
    llm_kind     TEXT,
    llm_fallback TEXT CHECK (llm_fallback IN ('wait', 'hub')),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (identity_id, agent_id),
    CHECK ((llm_route = 'hub') = (llm_kind IS NULL AND llm_fallback IS NULL))
);

-- Before and after are each the setting as JSON, NULL for none. `agent_id`
-- is NULL for the every-agent setting.
CREATE TABLE IF NOT EXISTS llm_route_changes (
    id          BIGSERIAL PRIMARY KEY,
    identity_id TEXT NOT NULL,
    agent_id    TEXT,
    before      TEXT,
    after       TEXT,
    actor       TEXT NOT NULL,
    at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS llm_route_changes_identity_at
    ON llm_route_changes (identity_id, at DESC);
