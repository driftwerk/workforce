-- The latest reading of each plan login: how full its windows were.
CREATE TABLE floor.plan_readings (
    login       TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    owner       TEXT,
    account     TEXT,
    plan        TEXT,
    windows     JSONB NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL
);
