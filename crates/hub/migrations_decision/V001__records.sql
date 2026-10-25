-- Decision records (docs/decision-backends-design.md, decision 14): what each
-- decision was asked and what came back, for training and judging an
-- install's own decision model. One table for every use. `input` holds what a
-- person or a step said, so nothing but the offline export reads it.
--
--
-- Beside it, who moved a workflow run by hand: kept here and not on the run,
-- so a replica from before this never reads, rewrites or renders one.
--
-- Two new tables and their indexes: no lock on anything that exists.

CREATE TABLE IF NOT EXISTS records (
    id            BIGSERIAL PRIMARY KEY,
    at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    usage         TEXT NOT NULL,
    agent_id      TEXT,
    person        TEXT,
    channel       TEXT,
    instance_id   TEXT,
    step_id       TEXT,
    -- The `workforce_decide::Input` the request was built from, and the
    -- characters it was built within.
    input         JSONB NOT NULL,
    request_chars INTEGER NOT NULL,
    backend       TEXT NOT NULL,
    model         TEXT NOT NULL,
    answers       JSONB NOT NULL,
    -- Whether the answer was acted on (false: shadow).
    acted         BOOLEAN NOT NULL,
    -- What the path beside the model decided, when one ran.
    verdict       TEXT
);

CREATE INDEX IF NOT EXISTS records_usage_at ON records (usage, at);
CREATE INDEX IF NOT EXISTS records_at ON records (at);
CREATE INDEX IF NOT EXISTS records_person ON records (person);

CREATE TABLE IF NOT EXISTS interventions (
    id            BIGSERIAL PRIMARY KEY,
    at            TIMESTAMPTZ NOT NULL,
    instance_id   TEXT NOT NULL,
    -- The person who made it, when a person did: what an erasure matches.
    person        TEXT,
    -- The `Intervention`: kind, actor, the steps it moved the run between,
    -- the outcome a resume gave and what the last step had chosen.
    intervention  JSONB NOT NULL
);

CREATE INDEX IF NOT EXISTS interventions_instance ON interventions (instance_id);
CREATE INDEX IF NOT EXISTS interventions_at ON interventions (at);
CREATE INDEX IF NOT EXISTS interventions_person ON interventions (person);
