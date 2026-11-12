-- How each engine was signed in, kept across restarts, so a call made
-- before Desk next reads the sign-in is still stamped with how it was paid
-- (not "unknown"), and every way each engine has been seen signed in: an
-- unknown call on an engine only ever seen on plans is a plan's.

-- The sign-in now, and since when Desk has seen it so.
CREATE TABLE ledger_sign_ins (
    engine TEXT        PRIMARY KEY,
    -- plan | key | vendor
    via    TEXT        NOT NULL,
    who    TEXT,
    since  TIMESTAMPTZ NOT NULL
);

-- Each way an engine was ever seen signed in, and when first.
CREATE TABLE ledger_billings_seen (
    engine TEXT        NOT NULL,
    via    TEXT        NOT NULL,
    since  TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (engine, via)
);
