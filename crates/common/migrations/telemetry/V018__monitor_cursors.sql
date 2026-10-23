-- Per-monitor catch-up: how far each monitor has read each channel it is bound
-- to. Replaces the per-(surface, channel) cursor in V008, under which one
-- monitor's resume skipped another's gap and a new monitor started from the
-- channel's older position. Still a READ OPTIMISATION ONLY — `monitor_dispatches`
-- is the source of truth, so a cursor may move without stranding work.
CREATE TABLE IF NOT EXISTS monitor_cursors (
    monitor_id     TEXT NOT NULL,
    owner          TEXT NOT NULL DEFAULT '__system__',
    surface        TEXT NOT NULL,
    channel        TEXT NOT NULL,
    last_swept_ts  TEXT NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (monitor_id, owner, surface, channel)
);

CREATE INDEX IF NOT EXISTS idx_monitor_cursors_channel
    ON monitor_cursors (surface, channel);

-- Every monitor adopts the position its channel had reached, so the first sweep
-- after the upgrade reads exactly what the old one would have: nothing re-fires
-- and nothing goes quiet. A monitor bound to `any` adopts the position of each
-- surface that was being swept. A monitor whose channel was never swept is seen
-- for the first time instead, and starts forward-only.
INSERT INTO monitor_cursors (monitor_id, owner, surface, channel, last_swept_ts)
SELECT d.id, d.owner_identity_id, c.surface, c.channel, c.last_swept_ts
  FROM workflow_defs d
  JOIN monitor_channel_cursors c
    ON c.channel = d.parsed_json->'monitor'->'binding'->>'channel'
   AND (d.parsed_json->'monitor'->'binding'->>'surface' IN ('any', c.surface))
 WHERE d.kind = 'monitor' AND d.parsed_json IS NOT NULL
ON CONFLICT (monitor_id, owner, surface, channel) DO NOTHING;

-- `monitor_channel_cursors` is left in place and unread by this version: a
-- rollback resumes from it and re-reads, which `monitor_dispatches` suppresses.
-- A later migration drops it once no replica runs the older code.
