-- A person's request their agent on another hub handed to their Desk boss
-- (DW-770): kept once by the agent's hub's id, with the boss it went to,
-- and the words owed back to where the person asked. Additive.
CREATE TABLE IF NOT EXISTS handed (
    id UUID PRIMARY KEY,
    agent TEXT NOT NULL,
    agent_name TEXT NOT NULL,
    place TEXT NOT NULL,
    words TEXT NOT NULL,
    session UUID,
    asked_at TIMESTAMPTZ NOT NULL,
    taken_at TIMESTAMPTZ NOT NULL,
    boss TEXT,
    delivered_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS handed_boss ON handed (boss, delivered_at);

CREATE TABLE IF NOT EXISTS handed_says (
    id BIGSERIAL PRIMARY KEY,
    handed UUID NOT NULL REFERENCES handed (id) ON DELETE CASCADE,
    text TEXT NOT NULL,
    at TIMESTAMPTZ NOT NULL,
    sent_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS handed_says_unsent ON handed_says (at) WHERE sent_at IS NULL;
