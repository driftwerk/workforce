-- A plan's goal made measurable (designs/boss-keeps-goals.md): the result
-- that alone closes it, the person's own words that agreed it, its
-- read-back, and when and how it was met. Null on every plan there is:
-- those keep today's meaning.
ALTER TABLE plans ADD COLUMN IF NOT EXISTS result JSONB;
ALTER TABLE plans ADD COLUMN IF NOT EXISTS agreed TEXT;
ALTER TABLE plans ADD COLUMN IF NOT EXISTS agreed_at TIMESTAMPTZ;
ALTER TABLE plans ADD COLUMN IF NOT EXISTS read_back_at TIMESTAMPTZ;
ALTER TABLE plans ADD COLUMN IF NOT EXISTS goal_ticket UUID REFERENCES tickets (id) ON DELETE SET NULL;
ALTER TABLE plans ADD COLUMN IF NOT EXISTS met_at TIMESTAMPTZ;
-- 'result' | 'person'
ALTER TABLE plans ADD COLUMN IF NOT EXISTS met_by TEXT;
ALTER TABLE plans ADD COLUMN IF NOT EXISTS met_why TEXT;
CREATE INDEX IF NOT EXISTS plans_open_goals ON plans (state)
    WHERE result IS NOT NULL AND met_at IS NULL;
-- An ask of the boss's about a goal (crew.ask_person with plan_id), the
-- change it proposes, and when it proposed it: the person's answer must
-- be newer, and the change applied must be this one.
ALTER TABLE boss_open_items ADD COLUMN IF NOT EXISTS plan UUID;
ALTER TABLE boss_open_items ADD COLUMN IF NOT EXISTS proposal JSONB;
ALTER TABLE boss_open_items ADD COLUMN IF NOT EXISTS proposed_at TIMESTAMPTZ;
-- When the change it proposed was applied: an ask is answered once, open
-- or closed (the person's answer on its buttons closes it first).
ALTER TABLE boss_open_items ADD COLUMN IF NOT EXISTS applied_at TIMESTAMPTZ;
CREATE INDEX IF NOT EXISTS boss_open_items_plan ON boss_open_items (plan)
    WHERE plan IS NOT NULL;
-- Each reading of a measured part: its value (none when it couldn't be
-- read, then `failed` says why), whether it met it, what it was read from.
CREATE TABLE IF NOT EXISTS plan_readings (
    plan     UUID             NOT NULL REFERENCES plans (id) ON DELETE CASCADE,
    at       TIMESTAMPTZ      NOT NULL,
    part     SMALLINT         NOT NULL,
    value    DOUBLE PRECISION,
    met      BOOLEAN          NOT NULL DEFAULT FALSE,
    words    TEXT             NOT NULL,
    evidence JSONB            NOT NULL DEFAULT '[]',
    failed   TEXT,
    PRIMARY KEY (plan, at, part)
);
-- What the boss reported (a fact) or showed toward a judged part (a
-- checkpoint), kept with the goal so no compaction or fresh thread loses it.
-- 'fact' | 'checkpoint'
CREATE TABLE IF NOT EXISTS plan_facts (
    id       UUID        PRIMARY KEY,
    plan     UUID        NOT NULL REFERENCES plans (id) ON DELETE CASCADE,
    kind     TEXT        NOT NULL,
    part     SMALLINT,
    at       TIMESTAMPTZ NOT NULL,
    text     TEXT        NOT NULL,
    evidence JSONB       NOT NULL DEFAULT '[]',
    checked  BOOLEAN     NOT NULL DEFAULT FALSE,
    made_by  JSONB       NOT NULL,
    replaces UUID        REFERENCES plan_facts (id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS plan_facts_plan ON plan_facts (plan, at DESC);
