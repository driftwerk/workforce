-- What each of the person's machines last said one boss's grant there spent
-- today, for a boss on this hub at Act fully: one daily budget is counted
-- across this computer and every grant of the boss on every machine (its
-- group's, and each of its plans'), so two grants on one machine add up.
-- Kept after a grant goes: that money was spent. Additive: `machine_spent`,
-- each machine's one count before grants were told apart, is still read, as
-- what that machine spent in all.
CREATE TABLE IF NOT EXISTS machine_grant_spent (
    agent      TEXT             NOT NULL,
    machine    TEXT             NOT NULL,
    grant_id   TEXT             NOT NULL,
    count_from TIMESTAMPTZ      NOT NULL,
    usd        DOUBLE PRECISION NOT NULL,
    updated_at TIMESTAMPTZ      NOT NULL,
    PRIMARY KEY (agent, machine, grant_id, count_from)
);
