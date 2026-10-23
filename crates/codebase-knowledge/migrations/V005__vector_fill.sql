-- How far the fill of a repo's code vectors from the graph has got, so
-- that one stopped part of the way goes on from there. `generation` is
-- stamped on every vector written for the repo from the fill's start; what
-- the store holds of the repo under another is stale, and removed when the
-- fill ends. `collection` is the making of the store's collection being
-- filled: a fill of a collection that is no more starts again.
CREATE TABLE IF NOT EXISTS vector_fill (
    repo_id    TEXT PRIMARY KEY,
    generation TEXT NOT NULL,
    collection TEXT NOT NULL,
    stage      TEXT NOT NULL,
    cursor     TEXT NOT NULL DEFAULT '',
    done       BIGINT NOT NULL DEFAULT 0,
    total      BIGINT NOT NULL DEFAULT 0
);

-- Files whose vectors may be behind the graph: noted before an index run
-- changes the graph for them, and taken off once the vector store has
-- them, so that a run that is killed, or whose vector write fails, leaves
-- them to be put right, file by file. `seq` grows each time a file is
-- noted, so that a note made since is not taken off with an earlier one.
CREATE TABLE IF NOT EXISTS vector_pending (
    repo_id   TEXT NOT NULL,
    file_path TEXT NOT NULL,
    seq       BIGINT NOT NULL DEFAULT 1,
    PRIMARY KEY (repo_id, file_path)
);

-- A fill pages through a repo's entities in id order, by an index on
-- (repo_id, id). It is not built here: a migration runs in the transaction
-- a hub boots in, and building it over millions of entities there would
-- hold the boot, and every writer of `entities`, for as long as it took
-- (4 to 5 seconds over 4.2 million rows on a laptop, more on a slower
-- disk). The hub builds it after it has started, CONCURRENTLY, and fills
-- page by the primary key until it is there (see
-- `PostgresCodeGraphBackend::ensure_fill_index`).
