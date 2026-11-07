-- Which Anthropic account each of the person's Claude accounts is, when
-- known: a hash of its organization and email (never either), the email to
-- show, and how it came to be known (the person said so, or its usage
-- matched this computer's own sign-in). A token names no account, so this
-- is never read from it.
CREATE TABLE engine_account_identities (
    account    UUID        PRIMARY KEY REFERENCES engine_accounts (id) ON DELETE CASCADE,
    identity   TEXT        NOT NULL,
    email      TEXT,
    linked_by  TEXT        NOT NULL,
    linked_at  TIMESTAMPTZ NOT NULL
);
