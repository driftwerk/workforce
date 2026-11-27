-- Requests an agent on another hub handed to the person's Desk boss
-- (DW-770), and the words owed back on them.
CREATE TABLE IF NOT EXISTS handed (
    agent TEXT NOT NULL,
    id UUID NOT NULL,
    agent_name TEXT NOT NULL,
    place TEXT NOT NULL,
    words TEXT NOT NULL,
    session TEXT,
    asked_at TIMESTAMPTZ NOT NULL,
    taken_at TIMESTAMPTZ NOT NULL,
    boss TEXT,
    delivered_at TIMESTAMPTZ,
    -- When it agreed a plan's goal: it agrees no other after.
    used_at TIMESTAMPTZ,
    PRIMARY KEY (agent, id)
);

CREATE INDEX IF NOT EXISTS handed_boss ON handed (boss, delivered_at);
CREATE INDEX IF NOT EXISTS handed_taken ON handed (taken_at);

CREATE TABLE IF NOT EXISTS handed_says (
    id BIGSERIAL PRIMARY KEY,
    agent TEXT NOT NULL,
    handed UUID NOT NULL,
    text TEXT NOT NULL,
    at TIMESTAMPTZ NOT NULL,
    tries INT NOT NULL DEFAULT 0,
    sent_at TIMESTAMPTZ,
    FOREIGN KEY (agent, handed) REFERENCES handed (agent, id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS handed_says_unsent ON handed_says (agent, at) WHERE sent_at IS NULL;
