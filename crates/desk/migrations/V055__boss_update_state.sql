-- A boss's updates (overseer H7): one row per boss, so a restart neither
-- loses a gathered update nor posts it twice.
--   since           when the boss started telling: older work isn't news
--   gather_since    the first change of the update being gathered
--   result          whether a visible result is in it (it goes sooner)
--   last_update_at  when the last update went out
--   unread          updates in a row the person hadn't read as it went
--   told            what each session's account said when last told
--   pending_id      an update written and about to go out, by its id
--   pending         that update: its text and extras, and what it tells
CREATE TABLE boss_update_state (
    boss           TEXT PRIMARY KEY,
    since          TIMESTAMPTZ NOT NULL,
    gather_since   TIMESTAMPTZ,
    result         BOOLEAN NOT NULL DEFAULT false,
    last_update_at TIMESTAMPTZ,
    unread         INT NOT NULL DEFAULT 0,
    told           JSONB NOT NULL DEFAULT '{}'::jsonb,
    pending_id     UUID,
    pending        JSONB
);
