-- A synced ticket's issue is unique within the link that imported or made
-- it, not across Desk: two clients' hubs may each have a project of one key
-- (ACME on Acme's Jira and on Globex's), each linked to a board of its own.
DROP INDEX IF EXISTS tickets_external;
CREATE UNIQUE INDEX tickets_external ON tickets (
    (external ->> 'system'), (external ->> 'link'), (external ->> 'key')
) WHERE external IS NOT NULL;
