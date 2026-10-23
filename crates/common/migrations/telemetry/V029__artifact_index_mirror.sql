-- Shadow mode (DW-676): a second artifact index, vellev's, kept beside the
-- one the hub serves searches from, and compared with it. Each index has
-- its own pending list, so neither keeper ever takes the other's notes.
--
-- The mirror's list is made like the serving one, with its key (the
-- trigger's ON CONFLICT needs it) and its defaults (noted_seq numbered
-- from the same sequence, which the keeper's own statements also name).
-- A later migration that changes artifact_index_pending changes this
-- table too.
CREATE TABLE IF NOT EXISTS artifact_index_pending_mirror
    (LIKE artifact_index_pending INCLUDING ALL);

-- Whether writes are noted for the mirror: a lease, one row. Writes are
-- noted while mirrored_until is in the future. Only a hub that keeps a
-- mirror renews it (artifact_mirror_renew), every 30 s for 90 s; a hub that
-- keeps none never touches it. So no hub can turn noting off for another,
-- and when no hub keeps a mirror any more, noting stops by itself within
-- one span. Writes made while the lease had lapsed are found by the mirror
-- keeper's next compare.
--
-- It gates the noting of writes alone. The operator's pause of the
-- shadow's reads is a switch of its own: a pause must not stop writes being
-- noted, or the mirror falls behind for the reconcile to repair.
--
-- Maintenance: the trigger reads this table, and notes into the mirror's
-- list, on every artifact write. Any later migration or maintenance that
-- touches either runs with a short lock_timeout, so that artifact writes
-- never wait on it; and before dropping either, let the lease lapse (stop
-- every mirror keeper and wait one span) or end it
-- (UPDATE shadow_control SET mirrored_until = NULL).
CREATE TABLE IF NOT EXISTS shadow_control (
    only_row       BOOLEAN PRIMARY KEY DEFAULT true CHECK (only_row),
    mirrored_until TIMESTAMPTZ
);
INSERT INTO shadow_control (only_row) VALUES (true) ON CONFLICT DO NOTHING;

-- Renew the lease to at least now() + span, in the schema of the control
-- row. Returns whether it had lapsed (never held, ended, or past) and its
-- new end, both on the database's clock, so that no hub's clock decides
-- either. Refused while the mirror's list is missing there, so that the
-- lease cannot ask the trigger to note into a table that is not there.
-- Never shortens a lease another hub renewed for longer.
CREATE OR REPLACE FUNCTION artifact_mirror_renew(span INTERVAL)
    RETURNS TABLE (had_lapsed BOOLEAN, held_until TIMESTAMPTZ) AS $$
DECLARE
    here TEXT := (SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                  WHERE c.oid = 'shadow_control'::regclass);
    was  TIMESTAMPTZ;
BEGIN
    IF to_regclass(format('%I.artifact_index_pending_mirror', here)) IS NULL THEN
        RAISE EXCEPTION 'the artifact mirror''s pending list (%.artifact_index_pending_mirror) is missing: it cannot be noted into', here;
    END IF;
    -- Locked as read: a change made between this read and the upsert
    -- below (a lease ended by hand) would otherwise be replaced by the
    -- upsert unseen, a lapse healed and never said.
    SELECT mirrored_until INTO was FROM shadow_control FOR UPDATE;
    -- Whether or not the row is there: a row deleted by hand would
    -- otherwise leave the lease where it was.
    INSERT INTO shadow_control (only_row, mirrored_until) VALUES (true, now() + span)
        ON CONFLICT (only_row) DO UPDATE
            SET mirrored_until = GREATEST(shadow_control.mirrored_until, EXCLUDED.mirrored_until);
    RETURN QUERY SELECT (was IS NULL OR was <= now()), c.mirrored_until FROM shadow_control c;
END;
$$ LANGUAGE plpgsql;

-- As V025, and then the same noting for the mirror while its lease holds.
-- An artifact write never fails for the mirror's sake: with the lease held
-- and the mirror's list missing (dropped by hand), the mirror is not noted,
-- and its keeper, whose own statements name the list, fails loudly
-- instead.
CREATE OR REPLACE FUNCTION note_artifact_index_pending() RETURNS trigger AS $$
DECLARE
    noted_id   TEXT;
    noted_hash TEXT;
    mirrored   BOOLEAN;
BEGIN
    IF TG_OP = 'DELETE' THEN
        noted_id := OLD.id;
        noted_hash := '';
    ELSE
        IF TG_OP = 'UPDATE'
            AND NEW.title IS NOT DISTINCT FROM OLD.title
            AND NEW.tags IS NOT DISTINCT FROM OLD.tags
            AND NEW.content IS NOT DISTINCT FROM OLD.content THEN
            RETURN NULL;
        END IF;
        noted_id := NEW.id;
        noted_hash := md5(NEW.title || E'\x01' || NEW.tags || E'\x01' || NEW.content);
    END IF;
    -- In the artifacts table's own schema, whatever the session's
    -- search_path.
    EXECUTE format(
        'INSERT INTO %1$I.artifact_index_pending (artifact_id, text_hash)
         VALUES ($1, $2)
         ON CONFLICT (artifact_id) DO UPDATE
             SET text_hash = EXCLUDED.text_hash, noted_at = now(), tries = 0,
                 last_error = NULL,
                 noted_seq = nextval(%2$L::regclass),
                 first_failed_at = NULL, next_try_at = NULL',
        TG_TABLE_SCHEMA,
        format('%I.artifact_index_noted_seq', TG_TABLE_SCHEMA)
    ) USING noted_id, noted_hash;
    IF to_regclass(format('%I.shadow_control', TG_TABLE_SCHEMA)) IS NOT NULL THEN
        EXECUTE format('SELECT mirrored_until > now() FROM %I.shadow_control', TG_TABLE_SCHEMA)
            INTO mirrored;
    END IF;
    IF mirrored
        AND to_regclass(format('%I.artifact_index_pending_mirror', TG_TABLE_SCHEMA)) IS NOT NULL THEN
        EXECUTE format(
            'INSERT INTO %1$I.artifact_index_pending_mirror (artifact_id, text_hash)
             VALUES ($1, $2)
             ON CONFLICT (artifact_id) DO UPDATE
                 SET text_hash = EXCLUDED.text_hash, noted_at = now(), tries = 0,
                     last_error = NULL,
                     noted_seq = nextval(%2$L::regclass),
                     first_failed_at = NULL, next_try_at = NULL',
            TG_TABLE_SCHEMA,
            format('%I.artifact_index_noted_seq', TG_TABLE_SCHEMA)
        ) USING noted_id, noted_hash;
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
