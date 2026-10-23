-- The artifact index keeper retries every artifact the index did not take,
-- for ever, each at its own time (DW-620, keeper redesign). Three things
-- are kept with a pending artifact for that:
--
-- noted_seq        a number that every noting takes afresh. Whoever indexes
--                  the artifact removes the row only if it still carries
--                  the number read before the write, so a noting made
--                  meanwhile is never removed by a try that did not see it.
--                  A hash cannot say this: a text changed and changed back
--                  has the hash it had.
-- first_failed_at  when the index first failed to take this noting.
-- next_try_at      when it is to be tried again; NULL for at once.
--
-- noted_seq has a default, so that a hub of the release before, which
-- inserts rows without it, goes on working beside this one.

CREATE SEQUENCE IF NOT EXISTS artifact_index_noted_seq;

ALTER TABLE artifact_index_pending
    ADD COLUMN IF NOT EXISTS noted_seq BIGINT NOT NULL
        DEFAULT nextval('artifact_index_noted_seq'),
    ADD COLUMN IF NOT EXISTS first_failed_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS next_try_at TIMESTAMPTZ;

-- A new version of the artifact starts again: a number of its own, no
-- tries, no error, and due at once.
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
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
