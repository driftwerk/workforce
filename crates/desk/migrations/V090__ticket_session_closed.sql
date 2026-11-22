-- When the session a ticket is assigned to was closed or removed, here or
-- on the machine it ran on: from then it holds no slot of its plan. Gone
-- when the ticket goes to anyone else, or its session runs again. Additive:
-- null for every ticket but those of the sessions already closed here.
ALTER TABLE tickets ADD COLUMN session_closed_at TIMESTAMPTZ;

-- The tickets of sessions here closed before.
UPDATE tickets t SET session_closed_at = s.updated_at
FROM sessions s
WHERE t.assignee ->> 'kind' = 'session' AND t.assignee ->> 'id' = s.id::text
  AND s.state = 'closed';
