-- Fills and pending files are kept per vector database (`backend`, as
-- `[knowledge] vector_backend` names it: 'weaviate' or 'vellev'), so that
-- one configured as a standby is told of every file changed while another
-- is in use, and its notes are not taken off by work done for the other.
--
-- What is here already is vellev's: only a store that keeps track of its
-- vectors' completeness writes these tables, and vellev's is the only one.
-- The default is dropped once the rows are labelled, so that nothing is
-- written later without saying for which database.
ALTER TABLE vector_fill ADD COLUMN backend TEXT NOT NULL DEFAULT 'vellev';
ALTER TABLE vector_fill ALTER COLUMN backend DROP DEFAULT;
ALTER TABLE vector_fill DROP CONSTRAINT vector_fill_pkey;
ALTER TABLE vector_fill ADD PRIMARY KEY (backend, repo_id);

ALTER TABLE vector_pending ADD COLUMN backend TEXT NOT NULL DEFAULT 'vellev';
ALTER TABLE vector_pending ALTER COLUMN backend DROP DEFAULT;
ALTER TABLE vector_pending DROP CONSTRAINT vector_pending_pkey;
ALTER TABLE vector_pending ADD PRIMARY KEY (backend, repo_id, file_path);
