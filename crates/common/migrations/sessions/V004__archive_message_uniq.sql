-- One archive row per (session, message): dedupe anything already doubled
-- (keeping the earliest seq), then enforce it so both the runtime append and
-- the backfill can insert with ON CONFLICT DO NOTHING.
DELETE FROM session_archive a
    USING session_archive b
    WHERE a.session_id = b.session_id
      AND a.message_id = b.message_id
      AND a.seq > b.seq;

CREATE UNIQUE INDEX session_archive_message_uniq
    ON session_archive (session_id, message_id);
