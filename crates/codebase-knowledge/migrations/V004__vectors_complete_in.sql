-- Which making of the vector store's collection holds a vector for every
-- entity of the repo, as the graph has them: the collection's identity, as
-- the store tells it, or NULL when no collection is known to. A flag would
-- outlive the collection: one dropped, or left out of a restored backup,
-- comes back as another, and what is recorded here is then not of it.
-- Kept with the graph, which is reachable whenever it was just written, so
-- that a vector write that failed can always say the repo's vectors are no
-- longer complete.

ALTER TABLE index_state
    ADD COLUMN IF NOT EXISTS vectors_complete_in TEXT;
