-- Who let a charge go: 'person' (they took the session back) or 'boss'
-- (its boss let it go). Null for one released before this was kept, and
-- while it runs. Desk shows the person a session they took back as theirs
-- across restarts from it.
ALTER TABLE charges ADD COLUMN released_by TEXT;
