-- Artifacts whose vector index may be behind Postgres: every artifact
-- written or deleted, until an index is known to hold that version of it.
--
-- The row is written by a trigger, so in the transaction that writes the
-- artifact, whoever writes it: a hub that is killed before it reaches the
-- index, one whose index does not answer, and a hub of an older release
-- all leave the row behind them. A hub with an index puts each right and
-- removes the row, if it still names the version the index took.
--
-- text_hash is the hash of what is indexed (title, tags and content), as
-- KnowledgeStore computes it (artifact_store.rs, TEXT_HASH); '' for an
-- artifact deleted. tries counts failed attempts, so that an artifact the
-- index will not take does not keep newer ones waiting.

CREATE TABLE IF NOT EXISTS artifact_index_pending (
    artifact_id TEXT PRIMARY KEY,
    text_hash   TEXT NOT NULL,
    noted_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    tries       INTEGER NOT NULL DEFAULT 0
);

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
             SET text_hash = EXCLUDED.text_hash, noted_at = now(), tries = 0',
        TG_TABLE_SCHEMA
    ) USING noted_id, noted_hash;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS artifacts_note_index_pending ON artifacts;
CREATE TRIGGER artifacts_note_index_pending
    AFTER INSERT OR UPDATE OR DELETE ON artifacts
    FOR EACH ROW EXECUTE FUNCTION note_artifact_index_pending();
