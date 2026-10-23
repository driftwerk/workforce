CREATE TABLE budget_overrides (
    scope TEXT NOT NULL,
    key TEXT NOT NULL,
    daily_usd DOUBLE PRECISION NOT NULL,
    expires_at TIMESTAMPTZ,
    note TEXT,
    created_by TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (scope, key)
);

CREATE TABLE budget_override_audit (
    id BIGSERIAL PRIMARY KEY,
    action TEXT NOT NULL,
    scope TEXT NOT NULL,
    key TEXT NOT NULL,
    daily_usd DOUBLE PRECISION,
    expires_at TIMESTAMPTZ,
    actor TEXT NOT NULL,
    at TIMESTAMPTZ NOT NULL
);
