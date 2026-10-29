-- What Desk keeps of its own between runs: window layouts, grid positions,
-- selections. One JSON value per key; the hub never reads inside it.
CREATE TABLE state (
    key        TEXT        PRIMARY KEY,
    value      JSONB       NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);
