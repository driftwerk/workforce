-- Groups as client boundaries. A sidebar group may have a boss of its own
-- that sees only that group, so rows are stamped
-- with the group they were written in: an overseer reads its own scope's,
-- and moving an item later never moves what was written about it.
-- `group_members` keeps Ungrouped's members under the fixed id `ungrouped`,
-- which is no group: it is null in every stamp here.

-- The group an event was about when it was recorded: a ticket's board's,
-- else its subject's (a session or this hub's agent, `agent:local/<id>`).
ALTER TABLE activity ADD COLUMN group_id TEXT;
UPDATE activity a SET group_id = b.group_id
  FROM tickets t JOIN boards b ON b.id = t.board
 WHERE a.ticket = t.id AND b.group_id IS NOT NULL;
UPDATE activity a SET group_id = m.group_id
  FROM group_members m
 WHERE a.group_id IS NULL AND m.group_id <> 'ungrouped' AND a.subject IS NOT NULL
   AND m.item = CASE a.subject ->> 'kind'
                  WHEN 'session' THEN 'session:' || (a.subject ->> 'id')
                  WHEN 'agent' THEN 'agent:local/' || (a.subject ->> 'id')
                END;
CREATE INDEX activity_group ON activity (group_id, at DESC);

-- The group the session that made a call was in when it was kept.
ALTER TABLE ledger ADD COLUMN group_id TEXT;
UPDATE ledger l SET group_id = m.group_id
  FROM group_members m
 WHERE m.group_id <> 'ungrouped' AND m.item = 'session:' || l.session::text;
CREATE INDEX ledger_group ON ledger (group_id, at);

-- A line message is its session's group's: the person's and the overseer's
-- to a session take that session's.
UPDATE line_messages l SET group_id = m.group_id
  FROM group_members m
 WHERE l.group_id IS NULL AND m.group_id <> 'ungrouped'
   AND l.recipient ->> 'kind' = 'session'
   AND m.item = 'session:' || (l.recipient ->> 'id');

-- A room may belong to a group; `shared_with` are the hubs the person
-- agreed the room's conversation goes to. A room made before this already
-- goes to its members' hubs.
ALTER TABLE rooms ADD COLUMN group_id TEXT;
ALTER TABLE rooms ADD COLUMN shared_with JSONB NOT NULL DEFAULT '[]';
UPDATE rooms SET shared_with = COALESCE(
  (SELECT jsonb_agg(DISTINCT m ->> 'hub') FROM jsonb_array_elements(members) m), '[]');
