-- Why the index last failed to take a pending artifact, kept beside how
-- many times it has: so that an artifact set aside, or on its way to being,
-- can be named with its reason, by whichever hub asks.
--
-- A new version of the artifact starts again: its tries were already put
-- back to 0 when it is noted, and the reason goes with them.

ALTER TABLE artifact_index_pending
    ADD COLUMN IF NOT EXISTS last_error TEXT;

CREATE OR REPLACE FUNCTION note_artifact_index_pending() RETURNS trigger AS $$
DECLARE
    noted_id   TEXT;
    noted_hash TEXT;
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
        'INSERT INTO %I.artifact_index_pending (artifact_id, text_hash)
         VALUES ($1, $2)
         ON CONFLICT (artifact_id) DO UPDATE
             SET text_hash = EXCLUDED.text_hash, noted_at = now(), tries = 0,
                 last_error = NULL',
        TG_TABLE_SCHEMA
    ) USING noted_id, noted_hash;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
