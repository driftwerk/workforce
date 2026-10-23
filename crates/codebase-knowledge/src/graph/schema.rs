use eyre::{Context, Result};
use rusqlite::Connection;
use tracing::info;

/// Database schema management for the code graph.
pub struct Schema;

/// Current schema version. Bump whenever a `V_N` step is added below
/// and the corresponding `ALTER`/`CREATE` is appended to
/// [`Schema::migrate`].
const CURRENT_VERSION: i64 = 6;

impl Schema {
    /// Create or migrate the schema to the latest version. Idempotent.
    ///
    /// Version is tracked via `PRAGMA user_version`. The full schema
    /// is created on first run; on subsequent runs each `V_N` step
    /// is applied only when the stored version is below it.
    ///
    /// The whole of it runs in one transaction that takes the database's
    /// write lock at its start, so that two processes opening one new or
    /// old database at once migrate it one after the other: the second
    /// finds it done.
    pub fn migrate(conn: &Connection) -> Result<()> {
        conn.execute_batch("BEGIN IMMEDIATE")
            .context("take the code graph's write lock to migrate it")?;
        match Self::migrate_locked(conn) {
            Ok(()) => conn.execute_batch("COMMIT").context("commit the migration"),
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn migrate_locked(conn: &Connection) -> Result<()> {
        conn.execute_batch(SCHEMA_V1)?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .context("read user_version")?;
        if version < 2 {
            apply_v2(conn).context("apply schema v2 (repositories.lineage)")?;
        }
        if version < 3 {
            apply_v3(conn).context("apply schema v3 (index_state.vectors_complete_in)")?;
        }
        if version < 4 {
            conn.execute_batch(SCHEMA_V4)
                .context("apply schema v4 (vector_fill, vector_pending)")?;
        }
        if version < 5 {
            conn.execute_batch(SCHEMA_V5)
                .context("apply schema v5 (graph_identity)")?;
        }
        if version < 6 {
            apply_v6(conn).context("apply schema v6 (vector state kept per backend)")?;
        }
        conn.execute(
            "INSERT OR IGNORE INTO graph_identity (only_one, id) VALUES (1, ?1)",
            [uuid::Uuid::new_v4().simple().to_string()],
        )?;
        conn.execute_batch(&format!("PRAGMA user_version = {CURRENT_VERSION};"))?;
        info!(version = CURRENT_VERSION, "Code graph schema initialised");
        Ok(())
    }
}

/// Add the `lineage` column to `repositories`. Guarded — SQLite
/// `ALTER TABLE ADD COLUMN` is not idempotent, and we don't use a
/// CHECK constraint because it can't be added by `ALTER`; validation
/// happens at the Rust boundary via `RepoLineage::from_str`.
fn apply_v2(conn: &Connection) -> Result<()> {
    let has_lineage: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('repositories') WHERE name = 'lineage'")?
        .exists([])?;
    if !has_lineage {
        conn.execute_batch(
            "ALTER TABLE repositories ADD COLUMN lineage TEXT NOT NULL DEFAULT 'unknown';
             CREATE INDEX IF NOT EXISTS idx_repositories_lineage ON repositories(lineage);",
        )?;
    }
    Ok(())
}

/// Add `vectors_complete_in` to `index_state`: which making of the vector
/// store's collection holds every entity of the repo. Guarded, as in
/// [`apply_v2`].
fn apply_v3(conn: &Connection) -> Result<()> {
    let has_column: bool = conn
        .prepare(
            "SELECT 1 FROM pragma_table_info('index_state') WHERE name = 'vectors_complete_in'",
        )?
        .exists([])?;
    if !has_column {
        conn.execute_batch("ALTER TABLE index_state ADD COLUMN vectors_complete_in TEXT;")?;
    }
    Ok(())
}

/// Keep fills and pending files per vector database, as Postgres's V007
/// has them: `backend` joins each table's key. SQLite cannot change a
/// primary key in place, so each table is made again and its rows copied
/// across, labelled `'vellev'`: only a store that keeps track of its
/// vectors' completeness writes them, and vellev's is the only one.
/// Guarded, as in [`apply_v2`]: a table that has `backend` is left be.
fn apply_v6(conn: &Connection) -> Result<()> {
    let keyed = |table: &str| -> Result<bool> {
        Ok(conn
            .prepare(&format!(
                "SELECT 1 FROM pragma_table_info('{table}') WHERE name = 'backend'"
            ))?
            .exists([])?)
    };
    if !keyed("vector_fill")? {
        conn.execute_batch(
            "CREATE TABLE vector_fill_keyed (
                backend    TEXT NOT NULL,
                repo_id    TEXT NOT NULL,
                generation TEXT NOT NULL,
                collection TEXT NOT NULL,
                stage      TEXT NOT NULL,
                cursor     TEXT NOT NULL DEFAULT '',
                done       INTEGER NOT NULL DEFAULT 0,
                total      INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (backend, repo_id)
             );
             INSERT INTO vector_fill_keyed
                 (backend, repo_id, generation, collection, stage, cursor, done, total)
             SELECT 'vellev', repo_id, generation, collection, stage, cursor, done, total
             FROM vector_fill;
             DROP TABLE vector_fill;
             ALTER TABLE vector_fill_keyed RENAME TO vector_fill;",
        )?;
    }
    if !keyed("vector_pending")? {
        conn.execute_batch(
            "CREATE TABLE vector_pending_keyed (
                backend   TEXT NOT NULL,
                repo_id   TEXT NOT NULL,
                file_path TEXT NOT NULL,
                seq       INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY (backend, repo_id, file_path)
             );
             INSERT INTO vector_pending_keyed (backend, repo_id, file_path, seq)
             SELECT 'vellev', repo_id, file_path, seq FROM vector_pending;
             DROP TABLE vector_pending;
             ALTER TABLE vector_pending_keyed RENAME TO vector_pending;",
        )?;
    }
    Ok(())
}

/// Which code graph this is: as Postgres's V006 has it.
const SCHEMA_V5: &str = r#"
CREATE TABLE IF NOT EXISTS graph_identity (
    only_one INTEGER PRIMARY KEY CHECK (only_one = 1),
    id       TEXT NOT NULL
);
"#;

/// How far the fill of a repo's vectors has got, and the files whose
/// vectors may be behind the graph: as Postgres's V005 has them.
const SCHEMA_V4: &str = r#"
CREATE TABLE IF NOT EXISTS vector_fill (
    repo_id    TEXT PRIMARY KEY,
    generation TEXT NOT NULL,
    collection TEXT NOT NULL,
    stage      TEXT NOT NULL,
    cursor     TEXT NOT NULL DEFAULT '',
    done       INTEGER NOT NULL DEFAULT 0,
    total      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS vector_pending (
    repo_id   TEXT NOT NULL,
    file_path TEXT NOT NULL,
    seq       INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (repo_id, file_path)
);
"#;

/// The index a fill pages a repo's entities by. Not part of the schema's
/// migration: that runs as the graph is opened, under the database's write
/// lock, and building this over millions of entities there holds up the
/// hub's start for as long as it takes (19 seconds over 4.2 million rows,
/// on a slow disk). It is built once the graph is open, from a connection
/// of its own; until it is there a fill pages by the primary key.
pub const FILL_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_entities_repo_id ON entities(repo_id, id);";

const SCHEMA_V1: &str = r#"
-- Repositories being tracked (supports multi-repo).
CREATE TABLE IF NOT EXISTS repositories (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    root_path TEXT NOT NULL,
    last_indexed_commit TEXT,
    last_indexed_at TEXT,
    created_at TEXT NOT NULL
);

-- Code entities (functions, structs, files, modules, etc.).
CREATE TABLE IF NOT EXISTS entities (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL REFERENCES repositories(id),
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    file_path TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    signature TEXT,
    visibility TEXT DEFAULT 'private',
    parent_id TEXT REFERENCES entities(id),
    doc_comment TEXT,
    content_hash TEXT NOT NULL,
    language TEXT NOT NULL,
    indexed_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_entities_repo ON entities(repo_id);
CREATE INDEX IF NOT EXISTS idx_entities_kind ON entities(kind);
CREATE INDEX IF NOT EXISTS idx_entities_file ON entities(file_path);
CREATE INDEX IF NOT EXISTS idx_entities_name ON entities(name);
CREATE INDEX IF NOT EXISTS idx_entities_qualified ON entities(qualified_name);

-- Relationships between entities.
CREATE TABLE IF NOT EXISTS relationships (
    id TEXT PRIMARY KEY,
    source_id TEXT NOT NULL REFERENCES entities(id),
    target_id TEXT NOT NULL REFERENCES entities(id),
    kind TEXT NOT NULL,
    confidence REAL DEFAULT 1.0,
    metadata TEXT,
    indexed_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_rel_source ON relationships(source_id);
CREATE INDEX IF NOT EXISTS idx_rel_target ON relationships(target_id);
CREATE INDEX IF NOT EXISTS idx_rel_kind ON relationships(kind);

-- LLM-generated summaries at various granularity levels.
CREATE TABLE IF NOT EXISTS summaries (
    entity_id TEXT NOT NULL REFERENCES entities(id),
    granularity TEXT NOT NULL,
    text TEXT NOT NULL,
    source_hash TEXT NOT NULL,
    model_used TEXT NOT NULL,
    generated_at TEXT NOT NULL,
    is_stale INTEGER DEFAULT 0,
    PRIMARY KEY (entity_id, granularity)
);

-- Index state for incremental tracking.
CREATE TABLE IF NOT EXISTS index_state (
    repo_id TEXT PRIMARY KEY REFERENCES repositories(id),
    last_commit TEXT,
    last_full_index_at TEXT,
    last_incremental_at TEXT,
    total_entities INTEGER DEFAULT 0,
    total_relationships INTEGER DEFAULT 0,
    indexing_in_progress INTEGER DEFAULT 0
);

-- File-level metadata for fast staleness checks.
CREATE TABLE IF NOT EXISTS file_index (
    repo_id TEXT NOT NULL,
    file_path TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    language TEXT NOT NULL,
    entity_count INTEGER DEFAULT 0,
    last_indexed_at TEXT NOT NULL,
    PRIMARY KEY (repo_id, file_path)
);

-- Active-run lease. One row per repo while indexing is in progress;
-- released (deleted) on completion. Survives process restarts so the
-- next run can detect interruptions via stale heartbeats and resume.
CREATE TABLE IF NOT EXISTS index_lease (
    repo_id              TEXT PRIMARY KEY REFERENCES repositories(id),
    worker_id            TEXT NOT NULL,
    target_commit        TEXT,
    phase                TEXT NOT NULL,
    acquired_at_ms       INTEGER NOT NULL,
    last_heartbeat_at_ms INTEGER NOT NULL,
    entities_total       INTEGER,
    entities_processed   INTEGER,
    summaries_total      INTEGER,
    summaries_processed  INTEGER
);

CREATE INDEX IF NOT EXISTS idx_index_lease_heartbeat
    ON index_lease(last_heartbeat_at_ms);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_creates_successfully() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        Schema::migrate(&conn).unwrap();

        // Verify tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"repositories".to_string()));
        assert!(tables.contains(&"entities".to_string()));
        assert!(tables.contains(&"relationships".to_string()));
        assert!(tables.contains(&"summaries".to_string()));
        assert!(tables.contains(&"index_state".to_string()));
        assert!(tables.contains(&"file_index".to_string()));
    }

    #[test]
    fn schema_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        Schema::migrate(&conn).unwrap();
        // Running again should not error
        Schema::migrate(&conn).unwrap();
    }

    #[test]
    fn schema_creates_lineage_column_with_default() {
        let conn = Connection::open_in_memory().unwrap();
        Schema::migrate(&conn).unwrap();
        let has_lineage: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('repositories') WHERE name = 'lineage'")
            .unwrap()
            .exists([])
            .unwrap();
        assert!(has_lineage);
        conn.execute(
            "INSERT INTO repositories (id, name, root_path, created_at)
             VALUES ('r', 'r', '/tmp', datetime('now'))",
            [],
        )
        .unwrap();
        let lineage: String = conn
            .query_row(
                "SELECT lineage FROM repositories WHERE id = 'r'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(lineage, "unknown");
    }

    #[test]
    fn schema_v1_to_v2_migration_adds_lineage() {
        // Simulate a pre-v2 install: tables created without `user_version`
        // and without the lineage column. Migrate should add the column
        // and bump user_version.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE repositories (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                root_path TEXT NOT NULL,
                last_indexed_commit TEXT,
                last_indexed_at TEXT,
                created_at TEXT NOT NULL
             );",
        )
        .unwrap();
        // Pre-existing row.
        conn.execute(
            "INSERT INTO repositories (id, name, root_path, created_at)
             VALUES ('old', 'old', '/old', '2025-01-01')",
            [],
        )
        .unwrap();
        Schema::migrate(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_VERSION);
        let lineage: String = conn
            .query_row(
                "SELECT lineage FROM repositories WHERE id = 'old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(lineage, "unknown");
    }

    #[test]
    fn schema_adds_vectors_complete_in_to_index_state_once() {
        let conn = Connection::open_in_memory().unwrap();
        Schema::migrate(&conn).unwrap();
        Schema::migrate(&conn).unwrap();
        let has_column: bool = conn
            .prepare(
                "SELECT 1 FROM pragma_table_info('index_state') WHERE name = 'vectors_complete_in'",
            )
            .unwrap()
            .exists([])
            .unwrap();
        assert!(has_column);
    }

    /// A graph from before vector state was kept per database (v5) has
    /// its fills and pending files labelled vellev's, every column kept;
    /// each table's key then takes the database, so that one repo and file
    /// may be noted for two; and migrating again changes nothing.
    #[test]
    fn v6_keeps_vector_state_per_backend_and_labels_what_was_there_vellevs() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        apply_v2(&conn).unwrap();
        apply_v3(&conn).unwrap();
        conn.execute_batch(SCHEMA_V4).unwrap();
        conn.execute_batch(SCHEMA_V5).unwrap();
        conn.execute_batch(
            "PRAGMA user_version = 5;
             INSERT INTO vector_fill (repo_id, generation, collection, stage, cursor, done, total)
             VALUES ('r', 'g1', 'making-1', 'entities', 'e7', 7, 9);
             INSERT INTO vector_pending (repo_id, file_path, seq) VALUES ('r', 'src/a.rs', 3);",
        )
        .unwrap();
        for _ in 0..2 {
            Schema::migrate(&conn).unwrap();
            let fill: (String, String, String, String, String, String, i64, i64) = conn
                .query_row(
                    "SELECT backend, repo_id, generation, collection, stage, cursor, done, total
                     FROM vector_fill",
                    [],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                        ))
                    },
                )
                .unwrap();
            assert_eq!(
                fill,
                (
                    "vellev".into(),
                    "r".into(),
                    "g1".into(),
                    "making-1".into(),
                    "entities".into(),
                    "e7".into(),
                    7,
                    9
                )
            );
            let pending: (String, String, String, i64) = conn
                .query_row(
                    "SELECT backend, repo_id, file_path, seq FROM vector_pending",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap();
            assert_eq!(pending, ("vellev".into(), "r".into(), "src/a.rs".into(), 3));
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap();
            assert_eq!(version, CURRENT_VERSION);
        }
        // Keyed by database: the same repo and file for another is a row of
        // its own; for the same, a conflict.
        conn.execute_batch(
            "INSERT INTO vector_fill (backend, repo_id, generation, collection, stage)
             VALUES ('weaviate', 'r', 'g2', 'm', 'entities');
             INSERT INTO vector_pending (backend, repo_id, file_path) VALUES ('weaviate', 'r', 'src/a.rs');",
        )
        .unwrap();
        assert!(conn
            .execute_batch(
                "INSERT INTO vector_pending (backend, repo_id, file_path)
                 VALUES ('vellev', 'r', 'src/a.rs');"
            )
            .is_err());
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM vector_pending", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 2);
    }

    /// The index fills page by is no part of the migration, which a hub
    /// waits for as it starts: the graph builds it once it is open.
    #[test]
    fn the_fill_index_is_built_after_the_graph_is_open_not_while_it_migrates() {
        assert!(!SCHEMA_V4.contains("CREATE INDEX"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codegraph.db");
        let indexed = |conn: &Connection| {
            conn.prepare("SELECT 1 FROM sqlite_master WHERE name = 'idx_entities_repo_id'")
                .unwrap()
                .exists([])
                .unwrap()
        };
        {
            let conn = Connection::open(&path).unwrap();
            Schema::migrate(&conn).unwrap();
            assert!(!indexed(&conn), "not by the migration");
        }
        let _graph = crate::graph::sqlite::SqliteCodeGraphBackend::open(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        let begun = std::time::Instant::now();
        while !indexed(&conn) {
            assert!(begun.elapsed() < std::time::Duration::from_secs(20));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Processes that open one database at once, new or of an older
    /// version, each find it migrated: none fails on a step another took.
    #[test]
    fn a_database_opened_by_several_at_once_is_migrated_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codegraph.db");
        // An older database: as version 2 left it.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            apply_v2(&conn).unwrap();
            conn.execute_batch("PRAGMA user_version = 2;").unwrap();
        }
        for _ in 0..5 {
            let opened: Vec<_> = (0..8)
                .map(|_| {
                    let path = path.clone();
                    std::thread::spawn(move || {
                        let conn = Connection::open(&path)?;
                        conn.execute_batch("PRAGMA busy_timeout=30000;")?;
                        Schema::migrate(&conn)?;
                        let id: String =
                            conn.query_row("SELECT id FROM graph_identity", [], |row| row.get(0))?;
                        eyre::Ok(id)
                    })
                })
                .collect();
            let ids: Vec<String> = opened
                .into_iter()
                .map(|opened| opened.join().unwrap().expect("migrated, or found migrated"))
                .collect();
            assert!(ids.iter().all(|id| *id == ids[0]), "one identity: {ids:?}");
            // Back to version 2 with the later columns still there, as a
            // process that died after altering and before recording would
            // leave it.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 2;").unwrap();
        }
    }
}
