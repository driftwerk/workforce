-- A service may hold several connections, each an instance of its adapter
-- (`skills/jira/workspaces/<instance>/…`): a check is kept per instance. The
-- connections kept before were each service's one flat connection, which
-- moves to the adapters' `default` instance, so that is what they backfill
-- to. Slack holds one connection only, as `default`.
ALTER TABLE connections ADD COLUMN instance TEXT NOT NULL DEFAULT 'default';
ALTER TABLE connections DROP CONSTRAINT connections_pkey;
ALTER TABLE connections ADD PRIMARY KEY (connector, level, agent, person, instance);
