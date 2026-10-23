-- The limits a descriptor sets, applied where the row is written.
--
-- In the database rather than in the write path for the same reason as the
-- search vector: no descriptor cache to invalidate, and no window where one
-- hub is still writing against limits another hub has already changed. The
-- cost is one lookup on `collections` by primary key per write, and only
-- collections that declared something pay anything beyond that.
CREATE FUNCTION enforce_collection_limits() RETURNS trigger AS $$
DECLARE
    d JSONB;
BEGIN
    SELECT descriptor INTO d FROM collections WHERE partition_key = NEW.partition_key;
    IF d IS NULL THEN
        RETURN NEW;
    END IF;

    IF d ? 'max_doc_bytes'
       AND octet_length(NEW.body::text) > (d ->> 'max_doc_bytes')::bigint THEN
        RAISE EXCEPTION 'document is % bytes; `%` allows %',
            octet_length(NEW.body::text), NEW.collection, d ->> 'max_doc_bytes'
            USING ERRCODE = 'check_violation';
    END IF;

    -- A collection-wide TTL is a default, so a per-write one still wins.
    IF NEW.expires_at IS NULL AND d ? 'ttl_secs' THEN
        NEW.expires_at := now() + make_interval(secs => (d ->> 'ttl_secs')::double precision);
    END IF;

    RETURN NEW;
END
$$ LANGUAGE plpgsql;

CREATE TRIGGER documents_collection_limits
    BEFORE INSERT OR UPDATE ON documents
    FOR EACH ROW EXECUTE FUNCTION enforce_collection_limits();
