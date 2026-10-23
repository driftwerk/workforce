-- The search vector for a body, decided by its collection's descriptor.
--
-- Doing this in the database rather than in the write path means there is no
-- descriptor cache to invalidate and no window where one hub indexes a
-- document by a field list another hub has already changed: the descriptor is
-- read in the same statement that writes the row.
--
-- Returns NULL where the collection declares no text_search — no row matches —
-- which is what leaves an undeclared collection storing nothing and paying
-- nothing.
CREATE FUNCTION document_search_vector(key TEXT, doc JSONB) RETURNS tsvector AS $$
    SELECT to_tsvector(
        COALESCE(c.descriptor -> 'text_search' ->> 'language', 'english')::regconfig,
        COALESCE(
            (SELECT string_agg(doc #>> string_to_array(f.value #>> '{}', '.'), ' ')
             FROM jsonb_array_elements(c.descriptor -> 'text_search' -> 'fields') f),
            ''
        )
    )
    FROM collections c
    WHERE c.partition_key = key
      AND c.descriptor ? 'text_search'
$$ LANGUAGE sql STABLE;

-- Documents written before their collection declared a search are indexed by
-- the same function when the descriptor is applied, so declaring a search does
-- not only cover what is written afterwards.
CREATE FUNCTION reindex_collection_search(key TEXT) RETURNS void AS $$
    UPDATE documents
       SET search_vec = document_search_vector(key, body)
     WHERE partition_key = key
$$ LANGUAGE sql;
