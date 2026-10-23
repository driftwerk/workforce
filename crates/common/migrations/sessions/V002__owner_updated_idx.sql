-- Owner-scoped listing (`list_for_owner`) backs the customer conversation
-- history and the recent-sessions digest; give its exact predicate an index
-- instead of riding `sessions_agent_updated` with a filtered owner.
CREATE INDEX IF NOT EXISTS sessions_owner_updated
    ON sessions (agent_id, owner, updated_at DESC);
