-- One row = the fleet-wide right to run a turn for a session. Written with a
-- conditional upsert (same owner, or expired) so exactly one instance holds a
-- live claim; rows are deleted on release and stealable after expiry.
CREATE TABLE turn_claims (
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    owner_instance TEXT NOT NULL,
    claim_id TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (agent_id, session_id)
);
