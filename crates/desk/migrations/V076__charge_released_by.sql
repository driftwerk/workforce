-- Who let a charge go: 'person' (they took the session back) or 'boss'
-- (its boss let it go, or Desk did for it). Null while it runs. A session
-- the person took back is left alone by every boss until they hand it to
-- one again, and Desk shows it as theirs across restarts.
ALTER TABLE charges ADD COLUMN released_by TEXT;

-- Charges let go before this was kept: who did, as Activity said it last.
UPDATE charges c
SET released_by = CASE WHEN a.actor ->> 'kind' = 'you' THEN 'person' ELSE 'boss' END
FROM (
    SELECT DISTINCT ON (subject ->> 'id') subject ->> 'id' AS session, actor
    FROM activity
    WHERE kind = 'released'
    ORDER BY subject ->> 'id', at DESC
) a
WHERE c.released_at IS NOT NULL
  AND c.released_by IS NULL
  AND a.session = c.session::text;
