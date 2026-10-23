//! Postgres backend for [`DocumentStore`]: one row per document in the
//! `store` schema. Every write runs in a transaction that locks the row
//! first, so the version and claim checks are decided against what is
//! actually there — the same way the in-memory backend decides them under
//! its lock — and a batch is all-or-nothing.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use eyre::{Context, Result};
use serde_json::Value;
use tokio::sync::OnceCell;
use tokio_postgres::types::ToSql;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use super::{
    anchor_of, check_batch, check_claim, check_write, content, encode_cursor, merge_shallow,
    partition_key, validate_body, validate_segment, CollectionInfo, ContentInfo, ContentSlice,
    ContentWrite, Descriptor, Direction, Document, DocumentStore, IndexKind, IndexSpec, Op, Page,
    Query, Resume, Scale, Scope, StoreError, Where, WriteOp, WriteOutcome,
};

// Own directory, not a subdir of `migrations/`: refinery's embed is
// recursive and a shared parent would fold this set into another runner's.
mod embedded {
    refinery::embed_migrations!("migrations_store");
}

pub const SCHEMA: &str = "store";

pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
    PostgresPool::from_config_with_session_init(config, &["SET search_path TO store, public"])
}

static STORE_POOL: OnceCell<PostgresPool> = OnceCell::const_new();

/// The process-wide store pool: built and migrated once. Fails closed — an
/// unreachable Postgres is an error, never a silent in-memory fall-back.
pub async fn shared_pool(config: &PostgresConfig) -> Result<PostgresPool> {
    STORE_POOL
        .get_or_try_init(|| async {
            let pool = build_pool(config).context("building document store pool")?;
            run_migrations(&pool).await?;
            Ok::<_, eyre::Report>(pool)
        })
        .await
        .cloned()
}

pub async fn run_migrations(pool: &PostgresPool) -> Result<()> {
    let set = MigrationSet::new(
        SCHEMA,
        Box::new(EmbeddedStoreMigrations) as Box<dyn MigrationRun>,
    );
    MigrationRunner::run(pool, &[set])
        .await
        .context("running document store migrations")
}

struct EmbeddedStoreMigrations;

#[async_trait]
impl MigrationRun for EmbeddedStoreMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

pub struct PostgresDocumentStore {
    pool: PostgresPool,
}

const COLUMNS: &str = "scope_kind, scope_id, collection, doc_id, body, version, created_by, \
                       created_at, updated_at, expires_at, lease_owner, lease_expires_at";

fn row_to_document(r: &tokio_postgres::Row) -> Result<Document> {
    let kind: String = r.get(0);
    let id: String = r.get(1);
    Ok(Document {
        scope: Scope::from_parts(&kind, &id)
            .ok_or_else(|| eyre::eyre!("unknown scope kind `{kind}` in store row"))?,
        collection: r.get(2),
        doc_id: r.get(3),
        body: r.get(4),
        version: r.get(5),
        created_by: r.get(6),
        created_at: r.get(7),
        updated_at: r.get(8),
        expires_at: r.get(9),
        lease_owner: r.get(10),
        lease_expires_at: r.get(11),
    })
}

/// tokio-postgres renders an `Error` as a bare "db error" and keeps what
/// actually went wrong in its source, so the cause is carried through here —
/// without it every failed statement reports the same useless string.
fn backend(e: impl std::error::Error) -> StoreError {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    StoreError::Backend(message)
}

/// `a.b.c` → the text[] path `#>` takes.
fn json_path(field: &str) -> Vec<String> {
    field.split('.').map(str::to_string).collect()
}

/// A Postgres identifier for something belonging to `parts`, short enough to
/// be one (63 bytes) and carrying none of the caller's text.
///
/// Collection names and field paths reach here from callers, and an
/// identifier cannot be a bound parameter. Hashing them means nothing a caller
/// writes is ever interpolated into DDL, so the question of quoting it does
/// not arise; `prefix` keeps the names greppable and lets the reconciler find
/// the indexes it owns.
fn ddl_name(prefix: &str, parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(&[0]);
    }
    format!(
        "{prefix}{}",
        hasher.finalize().to_hex().get(..16).unwrap_or("")
    )
}

fn partition_name(key: &str) -> String {
    ddl_name("documents_p_", &[key])
}

fn index_name(key: &str, spec: &IndexSpec) -> String {
    let kind = match spec.kind {
        IndexKind::Ordered => "o",
        IndexKind::Contains => "c",
    };
    ddl_name("docidx_", &[key, &spec.field, kind])
}

fn search_index_name(key: &str) -> String {
    ddl_name("docsearch_", &[key])
}

fn content_info_from_row(r: &tokio_postgres::Row) -> ContentInfo {
    ContentInfo {
        name: r.get(0),
        size_bytes: r.get(1),
        content_type: r.get(2),
        digest: r.get(3),
        version: r.get(4),
    }
}

/// The SQL an index spec becomes, with the table and index names already
/// resolved to hashes and the field path passed as a literal for Postgres to
/// quote through `format`.
///
/// An ordered index carries `doc_id` as its trailing column: the keyset
/// predicate seeks correctly without it, but the planner then adds a sort to
/// every page, because the index orders on the field alone while the query
/// orders on the tiebreaker too.
fn index_ddl(spec: &IndexSpec) -> &'static str {
    match spec.kind {
        IndexKind::Ordered => {
            "SELECT format('CREATE INDEX %I ON %I ((body -> %L), doc_id)', $1::text, $2::text, $3::text)"
        }
        IndexKind::Contains => {
            "SELECT format('CREATE INDEX %I ON %I USING GIN ((body -> %L) jsonb_path_ops)', $1::text, $2::text, $3::text)"
        }
    }
}

/// The instant a default-ordered anchor carries. `sort_key` writes it as an
/// RFC 3339 string, so anything else is a cursor from another ordering that
/// `Query::from_json` would have refused.
fn anchor_time(key: &Value) -> Result<DateTime<Utc>, StoreError> {
    key.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
        .ok_or_else(|| StoreError::Invalid("cursor does not belong to this ordering".into()))
}

/// One filter as a SQL predicate over `body`, appending its parameters.
/// Semantics match [`super::matches`]: comparisons only hold between values
/// of the same JSON type, `ne`/`not-in` hold for a missing field.
fn predicate(w: &Where, params: &mut Vec<Box<dyn ToSql + Sync + Send>>) -> String {
    let path_idx = {
        params.push(Box::new(json_path(&w.field)));
        params.len()
    };
    let at = format!("(body #> ${path_idx}::text[])");
    let value_idx = |params: &mut Vec<Box<dyn ToSql + Sync + Send>>, v: Value| {
        params.push(Box::new(v));
        params.len()
    };
    match w.op {
        Op::Eq => {
            let v = value_idx(params, w.value.clone());
            format!("{at} = ${v}::jsonb")
        }
        Op::Ne => {
            let v = value_idx(params, w.value.clone());
            format!("{at} IS DISTINCT FROM ${v}::jsonb")
        }
        Op::Lt | Op::Lte | Op::Gt | Op::Gte => {
            let sym = match w.op {
                Op::Lt => "<",
                Op::Lte => "<=",
                Op::Gt => ">",
                _ => ">=",
            };
            match &w.value {
                Value::Number(_) => {
                    let v = value_idx(params, w.value.clone());
                    format!(
                        "(jsonb_typeof({at}) = 'number' AND ({at})::numeric {sym} (${v}::jsonb)::numeric)"
                    )
                }
                Value::String(s) => {
                    params.push(Box::new(s.clone()));
                    let v = params.len();
                    format!(
                        "(jsonb_typeof({at}) = 'string' AND ({at} #>> '{{}}') {sym} ${v}::text)"
                    )
                }
                Value::Bool(b) => {
                    params.push(Box::new(*b));
                    let v = params.len();
                    format!(
                        "(jsonb_typeof({at}) = 'boolean' AND ({at})::boolean {sym} ${v}::boolean)"
                    )
                }
                // Arrays, objects and null have no order worth promising.
                _ => "FALSE".to_string(),
            }
        }
        Op::In => {
            let v = value_idx(params, w.value.clone());
            format!("{at} IN (SELECT jsonb_array_elements(${v}::jsonb))")
        }
        Op::NotIn => {
            let v = value_idx(params, w.value.clone());
            format!("NOT COALESCE({at} IN (SELECT jsonb_array_elements(${v}::jsonb)), FALSE)")
        }
        Op::ArrayContains => {
            let v = value_idx(params, w.value.clone());
            format!("(jsonb_typeof({at}) = 'array' AND {at} @> jsonb_build_array(${v}::jsonb))")
        }
    }
}

impl PostgresDocumentStore {
    /// Everything a scope holds goes, in the caller's transaction: its
    /// documents and content, its descriptors, and the partitions of its
    /// collections declared `large`.
    async fn drop_scope_in(
        tx: &deadpool_postgres::Transaction<'_>,
        scope: &Scope,
    ) -> Result<usize, StoreError> {
        let (kind, id) = (scope.kind(), scope.id());
        let prefix = format!("{kind}/{id}/");
        let partitions: Vec<String> = tx
            .query(
                "SELECT partition_key FROM collections WHERE scope_kind = $1 AND scope_id = $2",
                &[&kind, &id],
            )
            .await
            .map_err(backend)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let dropped = tx
            .execute(
                "DELETE FROM documents WHERE scope_kind = $1 AND scope_id = $2",
                &[&kind, &id],
            )
            .await
            .map_err(backend)?;
        for table in ["content_chunk", "content_meta"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE starts_with(partition_key, $1)"),
                &[&prefix],
            )
            .await
            .map_err(backend)?;
        }
        tx.execute(
            "DELETE FROM collections WHERE scope_kind = $1 AND scope_id = $2",
            &[&kind, &id],
        )
        .await
        .map_err(backend)?;
        for key in &partitions {
            Self::execute_built_ddl(
                tx,
                "SELECT format('DROP TABLE IF EXISTS %I', $1::text)",
                &[&partition_name(key)],
            )
            .await?;
        }
        Ok(usize::try_from(dropped).unwrap_or_default())
    }

    pub fn new(pool: PostgresPool) -> Self {
        Self { pool }
    }

    /// Run a `SELECT format(...)` that builds a DDL statement, then run what
    /// it returns. Postgres does the identifier and literal quoting through
    /// `format`'s `%I` and `%L`, which is the only reason it is safe to put a
    /// caller's field path into a statement that cannot take parameters.
    async fn execute_built_ddl(
        tx: &deadpool_postgres::Transaction<'_>,
        build: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<(), StoreError> {
        let statement: String = tx.query_one(build, params).await.map_err(backend)?.get(0);
        tx.batch_execute(&statement).await.map_err(backend)?;
        Ok(())
    }

    /// Move a collection into a partition of its own.
    ///
    /// A new partition cannot simply be attached while the default partition
    /// holds rows that would belong to it — Postgres refuses rather than move
    /// them — so the rows come out first, the partition is created, and they
    /// go back in. All inside the caller's transaction, so the collection is
    /// never half-moved.
    async fn give_own_partition(
        tx: &deadpool_postgres::Transaction<'_>,
        key: &str,
    ) -> Result<(), StoreError> {
        let name = partition_name(key);
        let exists: bool = tx
            .query_one(
                "SELECT to_regclass($1::text) IS NOT NULL",
                &[&format!("store.{name}")],
            )
            .await
            .map_err(backend)?
            .get(0);
        if exists {
            return Ok(());
        }

        tx.batch_execute("CREATE TEMP TABLE moving_documents (LIKE documents) ON COMMIT DROP")
            .await
            .map_err(backend)?;
        tx.execute(
            "WITH moved AS (DELETE FROM documents WHERE partition_key = $1 RETURNING *)
             INSERT INTO moving_documents SELECT * FROM moved",
            &[&key],
        )
        .await
        .map_err(backend)?;
        Self::execute_built_ddl(
            tx,
            "SELECT format('CREATE TABLE %I PARTITION OF documents FOR VALUES IN (%L)', $1::text, $2::text)",
            &[&name, &key],
        )
        .await?;
        tx.execute("INSERT INTO documents SELECT * FROM moving_documents", &[])
            .await
            .map_err(backend)?;
        Ok(())
    }

    /// Build the indexes a descriptor declares and drop the ones it no longer
    /// does, so applying a descriptor twice leaves exactly what it names.
    async fn reconcile_indexes(
        tx: &deadpool_postgres::Transaction<'_>,
        key: &str,
        descriptor: &Descriptor,
    ) -> Result<(), StoreError> {
        let table = partition_name(key);
        let wanted: Vec<(String, Option<IndexSpec>)> = descriptor
            .indexed_fields()
            .map(|spec| (index_name(key, spec), Some(spec.clone())))
            .chain(
                descriptor
                    .searches()
                    .map(|_| (search_index_name(key), None)),
            )
            .collect();

        let existing: Vec<String> = tx
            .query(
                "SELECT indexname FROM pg_indexes
                 WHERE schemaname = 'store' AND tablename = $1
                   AND (indexname LIKE 'docidx\\_%' OR indexname LIKE 'docsearch\\_%')",
                &[&table],
            )
            .await
            .map_err(backend)?
            .iter()
            .map(|r| r.get(0))
            .collect();

        for stale in existing
            .iter()
            .filter(|name| !wanted.iter().any(|(w, _)| w == *name))
        {
            Self::execute_built_ddl(tx, "SELECT format('DROP INDEX %I', $1::text)", &[stale])
                .await?;
        }

        for (name, spec) in wanted.iter().filter(|(n, _)| !existing.contains(n)) {
            match spec {
                Some(spec) => {
                    Self::execute_built_ddl(
                        tx,
                        index_ddl(spec),
                        &[name, &table, &spec.field],
                    )
                    .await?
                }
                None => {
                    Self::execute_built_ddl(
                        tx,
                        "SELECT format('CREATE INDEX %I ON %I USING GIN (search_vec)', $1::text, $2::text)",
                        &[name, &table],
                    )
                    .await?
                }
            }
        }
        Ok(())
    }

    /// The text search configuration a collection declared, or a refusal
    /// naming what to do about it. What is searchable is declared, the same
    /// way what is indexed is — a collection that declared nothing is not
    /// quietly scanned instead.
    async fn search_language(&self, collection: &str, scope: &Scope) -> Result<String, StoreError> {
        self.described(scope, collection)
            .await?
            .and_then(|info| info.descriptor.searches().map(|s| s.language.clone()))
            .ok_or_else(|| {
                StoreError::Invalid(format!(
                    "`{collection}` declares no text_search, so it cannot be searched — \
                     describe it with the fields to search first"
                ))
            })
    }

    fn info_from_row(row: &tokio_postgres::Row) -> Result<CollectionInfo, StoreError> {
        Ok(CollectionInfo {
            collection: row.get(0),
            descriptor: serde_json::from_value(row.get(1))
                .map_err(|e| StoreError::Backend(format!("stored descriptor: {e}")))?,
            revision: row.get(2),
        })
    }

    /// The write core: lock the row, decide against what is there, write.
    /// Mirrors `MemoryDocumentStore::apply` step for step.
    async fn apply(
        tx: &deadpool_postgres::Transaction<'_>,
        scope: &Scope,
        write: &WriteOp,
        actor: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<WriteOutcome, StoreError> {
        let (collection, doc_id) = (write.collection(), write.doc_id());
        let key = partition_key(scope, collection);
        let row = tx
            .query_opt(
                &format!(
                    "SELECT {COLUMNS} FROM documents
                     WHERE partition_key = $1 AND doc_id = $2
                     FOR UPDATE"
                ),
                &[&key, &doc_id],
            )
            .await
            .map_err(backend)?;
        let existing = match row {
            Some(r) => {
                let d = row_to_document(&r)?;
                if d.expires_at.is_some_and(|e| e <= now) {
                    // Expired rows read as absent; clear it so the write below
                    // is a plain insert.
                    tx.execute(
                        "DELETE FROM documents WHERE partition_key = $1 AND doc_id = $2",
                        &[&key, &doc_id],
                    )
                    .await
                    .map_err(backend)?;
                    None
                } else {
                    Some(d)
                }
            }
            None => None,
        };
        if let Some(doc) = &existing {
            if let Some((owner, until)) = doc.claimed_by_other(actor.unwrap_or(""), now) {
                return Err(StoreError::Claimed {
                    owner: owner.to_string(),
                    until,
                });
            }
        }
        let expect = |want: Option<i64>| -> Result<(), StoreError> {
            match (want, &existing) {
                (None, _) | (Some(0), None) => Ok(()),
                (Some(n), Some(d)) if d.version == n => Ok(()),
                (Some(n), Some(d)) => Err(StoreError::VersionConflict {
                    expected: n,
                    actual: d.version,
                }),
                (Some(n), None) => Err(StoreError::VersionConflict {
                    expected: n,
                    actual: 0,
                }),
            }
        };
        match write {
            WriteOp::Set {
                body,
                if_version,
                ttl_secs,
                ..
            } => {
                check_write(collection, doc_id, Some(body), *ttl_secs)?;
                expect(*if_version)?;
                let expires_at = ttl_secs.map(|t| now + Duration::seconds(t));
                let version = match &existing {
                    Some(d) => {
                        let version = d.version + 1;
                        tx.execute(
                            "UPDATE documents
                             SET body = $3, version = $4, updated_at = $5, expires_at = $6,
                                 search_vec = document_search_vector($1, $3::jsonb)
                             WHERE partition_key = $1 AND doc_id = $2",
                            &[&key, &doc_id, body, &version, &now, &expires_at],
                        )
                        .await
                        .map_err(backend)?;
                        version
                    }
                    None => {
                        tx.execute(
                            "INSERT INTO documents
                             (partition_key, doc_id, scope_kind, scope_id, collection, body,
                              version, created_by, created_at, updated_at, expires_at, search_vec)
                             VALUES ($1, $2, $3, $4, $5, $6, 1, $7, $8, $8, $9,
                                     document_search_vector($1, $6::jsonb))",
                            &[
                                &key,
                                &doc_id,
                                &scope.kind(),
                                &scope.id(),
                                &collection,
                                body,
                                &actor,
                                &now,
                                &expires_at,
                            ],
                        )
                        .await
                        .map_err(backend)?;
                        1
                    }
                };
                Ok(WriteOutcome::Written { version })
            }
            WriteOp::Update {
                body, if_version, ..
            } => {
                check_write(collection, doc_id, Some(body), None)?;
                if existing.is_none() {
                    return Err(StoreError::NotFound);
                }
                expect(*if_version)?;
                let current = existing.expect("checked above");
                let merged = merge_shallow(&current.body, body);
                validate_body(&merged)?;
                let version = current.version + 1;
                tx.execute(
                    "UPDATE documents SET body = $3, version = $4, updated_at = $5,
                            search_vec = document_search_vector($1, $3::jsonb)
                     WHERE partition_key = $1 AND doc_id = $2",
                    &[&key, &doc_id, &merged, &version, &now],
                )
                .await
                .map_err(backend)?;
                Ok(WriteOutcome::Written { version })
            }
            WriteOp::Delete { if_version, .. } => {
                check_write(collection, doc_id, None, None)?;
                if existing.is_none() {
                    if let Some(n) = if_version {
                        if *n != 0 {
                            return Err(StoreError::VersionConflict {
                                expected: *n,
                                actual: 0,
                            });
                        }
                    }
                    return Ok(WriteOutcome::Deleted { existed: false });
                }
                expect(*if_version)?;
                tx.execute(
                    "DELETE FROM documents WHERE partition_key = $1 AND doc_id = $2",
                    &[&key, &doc_id],
                )
                .await
                .map_err(backend)?;
                Ok(WriteOutcome::Deleted { existed: true })
            }
        }
    }

    async fn write_one(
        &self,
        scope: &Scope,
        write: WriteOp,
        actor: Option<&str>,
    ) -> Result<WriteOutcome, StoreError> {
        let mut conn = self.pool.get().await.map_err(backend)?;
        let tx = conn.transaction().await.map_err(backend)?;
        let out = Self::apply(&tx, scope, &write, actor, Utc::now()).await?;
        tx.commit().await.map_err(backend)?;
        Ok(out)
    }

    async fn fetch(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
    ) -> Result<Document, StoreError> {
        self.get(scope, collection, doc_id)
            .await?
            .ok_or(StoreError::NotFound)
    }

    async fn select(
        &self,
        scope: &Scope,
        collection: &str,
        query: &Query,
        filtered: bool,
    ) -> Result<Page> {
        let mut params: Vec<Box<dyn ToSql + Sync + Send>> =
            vec![Box::new(partition_key(scope, collection))];
        let mut sql = format!(
            "SELECT {COLUMNS} FROM documents
             WHERE partition_key = $1
               AND (expires_at IS NULL OR expires_at > now())"
        );
        if filtered {
            for w in &query.filters {
                let p = predicate(w, &mut params);
                sql.push_str(" AND ");
                sql.push_str(&p);
            }
        }
        if let Some(term) = &query.search {
            let language = self.search_language(collection, scope).await?;
            params.push(Box::new(language));
            let lang = params.len();
            params.push(Box::new(term.clone()));
            sql.push_str(&format!(
                " AND search_vec @@ websearch_to_tsquery(${lang}::text::regconfig, ${}::text)",
                params.len()
            ));
        }
        let dir = match query.direction {
            Direction::Asc => "ASC",
            Direction::Desc => "DESC",
        };
        // A missing field is ordered as JSON null rather than as SQL NULL, so
        // it holds a definite place in the order and a row that lacks the
        // field cannot vanish from a keyset walk. The in-memory backend reads
        // a missing field the same way.
        let order_expr = match &query.order_by {
            Some(field) => {
                params.push(Box::new(json_path(field)));
                format!("COALESCE(body #> ${}::text[], 'null'::jsonb)", params.len())
            }
            None => "updated_at".to_string(),
        };

        match query.resume() {
            Some(Resume::After(anchor)) => {
                let cmp = match query.direction {
                    Direction::Asc => ">",
                    Direction::Desc => "<",
                };
                let key = match &query.order_by {
                    Some(_) => {
                        params.push(Box::new(anchor.key.clone()));
                        format!("${}::jsonb", params.len())
                    }
                    None => {
                        params.push(Box::new(anchor_time(&anchor.key)?));
                        format!("${}::timestamptz", params.len())
                    }
                };
                params.push(Box::new(anchor.doc_id.clone()));
                sql.push_str(&format!(
                    " AND ({order_expr}, doc_id) {cmp} ({key}, ${})",
                    params.len()
                ));
            }
            Some(Resume::Offset(_)) | None => {}
        }

        sql.push_str(&format!(" ORDER BY {order_expr} {dir}, doc_id {dir}"));
        let size = query.page_size();
        // Offsets are the pre-keyset cursor encoding, honoured so a cursor
        // held across the upgrade keeps paging rather than restarting.
        let offset = match query.resume() {
            Some(Resume::Offset(n)) => n,
            _ => 0,
        };
        sql.push_str(&format!(" LIMIT {} OFFSET {offset}", size + 1));

        let conn = self.pool.get().await?;
        let refs: Vec<&(dyn ToSql + Sync)> = params
            .iter()
            .map(|p| p.as_ref() as &(dyn ToSql + Sync))
            .collect();
        let rows = conn.query(&sql, &refs).await?;
        let mut documents = rows
            .iter()
            .map(row_to_document)
            .collect::<Result<Vec<_>>>()?;
        let more = documents.len() > size;
        documents.truncate(size);
        Ok(Page {
            next_cursor: more
                .then(|| {
                    documents
                        .last()
                        .map(|d| encode_cursor(&anchor_of(d, query)))
                })
                .flatten(),
            documents,
        })
    }
}

#[async_trait]
impl DocumentStore for PostgresDocumentStore {
    async fn get(&self, scope: &Scope, collection: &str, doc_id: &str) -> Result<Option<Document>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                &format!(
                    "SELECT {COLUMNS} FROM documents
                     WHERE partition_key = $1 AND doc_id = $2
                       AND (expires_at IS NULL OR expires_at > now())"
                ),
                &[&partition_key(scope, collection), &doc_id],
            )
            .await?;
        row.as_ref().map(row_to_document).transpose()
    }

    async fn set(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        body: Value,
        if_version: Option<i64>,
        ttl_secs: Option<i64>,
        actor: Option<&str>,
    ) -> Result<Document, StoreError> {
        let write = WriteOp::Set {
            collection: collection.into(),
            doc_id: doc_id.into(),
            body,
            if_version,
            ttl_secs,
        };
        self.write_one(scope, write, actor).await?;
        self.fetch(scope, collection, doc_id).await
    }

    async fn update(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        patch: Value,
        if_version: Option<i64>,
        actor: Option<&str>,
    ) -> Result<Document, StoreError> {
        let write = WriteOp::Update {
            collection: collection.into(),
            doc_id: doc_id.into(),
            body: patch,
            if_version,
        };
        self.write_one(scope, write, actor).await?;
        self.fetch(scope, collection, doc_id).await
    }

    async fn delete(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        if_version: Option<i64>,
        actor: Option<&str>,
    ) -> Result<bool, StoreError> {
        let write = WriteOp::Delete {
            collection: collection.into(),
            doc_id: doc_id.into(),
            if_version,
        };
        match self.write_one(scope, write, actor).await? {
            WriteOutcome::Deleted { existed } => Ok(existed),
            WriteOutcome::Written { .. } => unreachable!("delete never writes"),
        }
    }

    async fn list(&self, scope: &Scope, collection: &str, query: &Query) -> Result<Page> {
        self.select(scope, collection, query, false).await
    }

    async fn query(&self, scope: &Scope, collection: &str, query: &Query) -> Result<Page> {
        self.select(scope, collection, query, true).await
    }

    async fn batch(
        &self,
        scope: &Scope,
        writes: &[WriteOp],
        actor: Option<&str>,
    ) -> Result<Vec<WriteOutcome>, StoreError> {
        check_batch(writes)?;
        let mut conn = self.pool.get().await.map_err(backend)?;
        let tx = conn.transaction().await.map_err(backend)?;
        let now = Utc::now();
        let mut outcomes = Vec::with_capacity(writes.len());
        for w in writes {
            // An error drops `tx` unfinished, which rolls the batch back.
            outcomes.push(Self::apply(&tx, scope, w, actor, now).await?);
        }
        tx.commit().await.map_err(backend)?;
        Ok(outcomes)
    }

    async fn claim(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        owner: &str,
        ttl_secs: i64,
    ) -> Result<Document, StoreError> {
        check_claim(ttl_secs, owner)?;
        let now = Utc::now();
        let mut conn = self.pool.get().await.map_err(backend)?;
        let tx = conn.transaction().await.map_err(backend)?;
        let row = tx
            .query_opt(
                &format!(
                    "SELECT {COLUMNS} FROM documents
                     WHERE partition_key = $1 AND doc_id = $2
                       AND (expires_at IS NULL OR expires_at > now())
                     FOR UPDATE"
                ),
                &[&partition_key(scope, collection), &doc_id],
            )
            .await
            .map_err(backend)?;
        let doc = row
            .as_ref()
            .map(row_to_document)
            .transpose()?
            .ok_or(StoreError::NotFound)?;
        if let Some((holder, until)) = doc.claimed_by_other(owner, now) {
            return Err(StoreError::Claimed {
                owner: holder.to_string(),
                until,
            });
        }
        let until = now + Duration::seconds(ttl_secs);
        tx.execute(
            "UPDATE documents SET lease_owner = $3, lease_expires_at = $4
             WHERE partition_key = $1 AND doc_id = $2",
            &[&partition_key(scope, collection), &doc_id, &owner, &until],
        )
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(Document {
            lease_owner: Some(owner.to_string()),
            lease_expires_at: Some(until),
            ..doc
        })
    }

    async fn release(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        owner: &str,
    ) -> Result<bool, StoreError> {
        let conn = self.pool.get().await.map_err(backend)?;
        let changed = conn
            .execute(
                "UPDATE documents SET lease_owner = NULL, lease_expires_at = NULL
                 WHERE partition_key = $1 AND doc_id = $2 AND lease_owner = $3",
                &[&partition_key(scope, collection), &doc_id, &owner],
            )
            .await
            .map_err(backend)?;
        Ok(changed == 1)
    }

    async fn describe(
        &self,
        scope: &Scope,
        collection: &str,
        descriptor: &Descriptor,
    ) -> Result<CollectionInfo, StoreError> {
        validate_segment("collection", collection)?;
        if let Some(search) = descriptor.searches() {
            let known: bool = self
                .pool
                .get()
                .await
                .map_err(backend)?
                .query_one(
                    "SELECT EXISTS (SELECT 1 FROM pg_ts_config WHERE cfgname::text = $1::text)",
                    &[&search.language],
                )
                .await
                .map_err(backend)?
                .get(0);
            if !known {
                return Err(StoreError::Invalid(format!(
                    "`{}` is not a text search configuration this server knows",
                    search.language
                )));
            }
        }

        let key = partition_key(scope, collection);
        let mut conn = self.pool.get().await.map_err(backend)?;
        let tx = conn.transaction().await.map_err(backend)?;

        let stored = serde_json::to_value(descriptor)
            .map_err(|e| StoreError::Invalid(format!("descriptor: {e}")))?;
        let row = tx
            .query_one(
                "INSERT INTO collections
                   (partition_key, scope_kind, scope_id, collection, descriptor)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (partition_key) DO UPDATE
                   SET descriptor = EXCLUDED.descriptor,
                       updated_at = now(),
                       revision = collections.revision
                                + (collections.descriptor IS DISTINCT FROM EXCLUDED.descriptor)::int
                 RETURNING collection, descriptor, revision",
                &[&key, &scope.kind(), &scope.id(), &collection, &stored],
            )
            .await
            .map_err(backend)?;

        if descriptor.scale == Scale::Large {
            Self::give_own_partition(&tx, &key).await?;
            Self::reconcile_indexes(&tx, &key, descriptor).await?;
        }
        // Documents already written are indexed too, so declaring a search
        // covers what is there rather than only what comes next.
        tx.execute("SELECT reindex_collection_search($1)", &[&key])
            .await
            .map_err(backend)?;

        let info = Self::info_from_row(&row)?;
        tx.commit().await.map_err(backend)?;
        Ok(info)
    }

    async fn described(
        &self,
        scope: &Scope,
        collection: &str,
    ) -> Result<Option<CollectionInfo>, StoreError> {
        self.pool
            .get()
            .await
            .map_err(backend)?
            .query_opt(
                "SELECT collection, descriptor, revision FROM collections WHERE partition_key = $1",
                &[&partition_key(scope, collection)],
            )
            .await
            .map_err(backend)?
            .as_ref()
            .map(Self::info_from_row)
            .transpose()
    }

    async fn collections(&self, scope: &Scope) -> Result<Vec<CollectionInfo>, StoreError> {
        self.pool
            .get()
            .await
            .map_err(backend)?
            .query(
                "SELECT collection, descriptor, revision FROM collections
                 WHERE scope_kind = $1 AND scope_id = $2
                 ORDER BY collection",
                &[&scope.kind(), &scope.id()],
            )
            .await
            .map_err(backend)?
            .iter()
            .map(Self::info_from_row)
            .collect()
    }

    async fn content_write(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
        offset: i64,
        bytes: &[u8],
        content_type: Option<&str>,
    ) -> Result<ContentWrite, StoreError> {
        validate_segment("name", name)?;
        let key = partition_key(scope, collection);
        let mut conn = self.pool.get().await.map_err(backend)?;
        let tx = conn.transaction().await.map_err(backend)?;

        // Lock the stream's row so two writers cannot both believe they are
        // appending at the same offset.
        let current = tx
            .query_opt(
                "SELECT pending, pending_bytes, version FROM content_meta
                 WHERE partition_key = $1 AND doc_id = $2 AND name = $3
                 FOR UPDATE",
                &[&key, &doc_id, &name],
            )
            .await
            .map_err(backend)?;

        let (pending, pending_bytes, latest) = match &current {
            Some(row) => (
                row.get::<_, Option<i64>>(0),
                row.get::<_, i64>(1),
                row.get::<_, Option<i64>>(2).unwrap_or(0),
            ),
            None => (None, 0, 0),
        };
        content::check_write(offset, bytes, pending_bytes)?;

        // A write with nothing pending opens a new version; one already open
        // continues it. Either way the bytes are invisible until commit.
        let version = pending.unwrap_or(latest + 1);
        if pending.is_none() {
            // An abandoned upload's chunks are replaced, not accumulated.
            tx.execute(
                "DELETE FROM content_chunk
                 WHERE partition_key = $1 AND doc_id = $2 AND name = $3 AND version = $4",
                &[&key, &doc_id, &name, &version],
            )
            .await
            .map_err(backend)?;
        }

        tx.execute(
            "INSERT INTO content_chunk
               (partition_key, doc_id, name, version, start_offset, byte_len, bytes)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            &[
                &key,
                &doc_id,
                &name,
                &version,
                &offset,
                &(bytes.len() as i64),
                &bytes,
            ],
        )
        .await
        .map_err(backend)?;

        let next_offset = offset + bytes.len() as i64;
        tx.execute(
            "INSERT INTO content_meta
               (partition_key, doc_id, name, pending, pending_bytes, content_type)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (partition_key, doc_id, name) DO UPDATE
               SET pending = $4,
                   pending_bytes = $5,
                   content_type = COALESCE(EXCLUDED.content_type, content_meta.content_type),
                   updated_at = now()",
            &[&key, &doc_id, &name, &version, &next_offset, &content_type],
        )
        .await
        .map_err(backend)?;

        tx.commit().await.map_err(backend)?;
        Ok(ContentWrite {
            version,
            next_offset,
        })
    }

    async fn content_commit(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
    ) -> Result<ContentInfo, StoreError> {
        let key = partition_key(scope, collection);
        let mut conn = self.pool.get().await.map_err(backend)?;
        let tx = conn.transaction().await.map_err(backend)?;

        let row = tx
            .query_opt(
                "SELECT pending, pending_bytes, content_type, version FROM content_meta
                 WHERE partition_key = $1 AND doc_id = $2 AND name = $3
                 FOR UPDATE",
                &[&key, &doc_id, &name],
            )
            .await
            .map_err(backend)?
            .ok_or_else(|| StoreError::Invalid("nothing is pending under that name".into()))?;
        let pending: Option<i64> = row.get(0);
        let size_bytes: i64 = row.get(1);
        let content_type: Option<String> = row.get(2);
        let previous: Option<i64> = row.get(3);
        let version = pending
            .ok_or_else(|| StoreError::Invalid("nothing is pending under that name".into()))?;

        // Hash the bytes as they are read back, in order, so the digest
        // describes what was actually stored rather than what was offered.
        let mut hasher = blake3::Hasher::new();
        for chunk in tx
            .query(
                "SELECT bytes FROM content_chunk
                 WHERE partition_key = $1 AND doc_id = $2 AND name = $3 AND version = $4
                 ORDER BY start_offset",
                &[&key, &doc_id, &name, &version],
            )
            .await
            .map_err(backend)?
        {
            hasher.update(chunk.get::<_, &[u8]>(0));
        }
        let digest = hasher.finalize().to_hex().to_string();

        if let Some(previous) = previous {
            tx.execute(
                "DELETE FROM content_chunk
                 WHERE partition_key = $1 AND doc_id = $2 AND name = $3 AND version = $4",
                &[&key, &doc_id, &name, &previous],
            )
            .await
            .map_err(backend)?;
        }
        tx.execute(
            "UPDATE content_meta
               SET version = $4, pending = NULL, size_bytes = $5, pending_bytes = 0,
                   digest = $6, updated_at = now()
             WHERE partition_key = $1 AND doc_id = $2 AND name = $3",
            &[&key, &doc_id, &name, &version, &size_bytes, &digest],
        )
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;

        Ok(ContentInfo {
            name: name.to_string(),
            size_bytes,
            content_type,
            digest: Some(digest),
            version,
        })
    }

    async fn content_read(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
        offset: i64,
        len: i64,
    ) -> Result<Option<ContentSlice>, StoreError> {
        content::check_read(offset, len)?;
        let key = partition_key(scope, collection);
        let conn = self.pool.get().await.map_err(backend)?;
        let Some(meta) = conn
            .query_opt(
                "SELECT version, size_bytes FROM content_meta
                 WHERE partition_key = $1 AND doc_id = $2 AND name = $3 AND version IS NOT NULL",
                &[&key, &doc_id, &name],
            )
            .await
            .map_err(backend)?
        else {
            return Ok(None);
        };
        let version: i64 = meta.get(0);
        let size_bytes: i64 = meta.get(1);
        let end = offset.saturating_add(len);

        // Only the chunks overlapping the range are read, which is what makes
        // this a range read rather than a scan of the whole stream.
        let mut bytes = vec![0u8; 0];
        for row in conn
            .query(
                "SELECT start_offset, bytes FROM content_chunk
                 WHERE partition_key = $1 AND doc_id = $2 AND name = $3 AND version = $4
                   AND start_offset < $5 AND start_offset + byte_len > $6
                 ORDER BY start_offset",
                &[&key, &doc_id, &name, &version, &end, &offset],
            )
            .await
            .map_err(backend)?
        {
            let start: i64 = row.get(0);
            let chunk: &[u8] = row.get(1);
            let from = offset.saturating_sub(start).max(0) as usize;
            let to = ((end - start) as usize).min(chunk.len());
            if from < to {
                bytes.extend_from_slice(&chunk[from..to]);
            }
        }
        Ok(Some(ContentSlice {
            offset,
            bytes,
            size_bytes,
        }))
    }

    async fn content_stat(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
    ) -> Result<Option<ContentInfo>, StoreError> {
        Ok(self
            .pool
            .get()
            .await
            .map_err(backend)?
            .query_opt(
                "SELECT name, size_bytes, content_type, digest, version FROM content_meta
                 WHERE partition_key = $1 AND doc_id = $2 AND name = $3 AND version IS NOT NULL",
                &[&partition_key(scope, collection), &doc_id, &name],
            )
            .await
            .map_err(backend)?
            .as_ref()
            .map(content_info_from_row))
    }

    async fn content_list(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
    ) -> Result<Vec<ContentInfo>, StoreError> {
        Ok(self
            .pool
            .get()
            .await
            .map_err(backend)?
            .query(
                "SELECT name, size_bytes, content_type, digest, version FROM content_meta
                 WHERE partition_key = $1 AND doc_id = $2 AND version IS NOT NULL
                 ORDER BY name",
                &[&partition_key(scope, collection), &doc_id],
            )
            .await
            .map_err(backend)?
            .iter()
            .map(content_info_from_row)
            .collect())
    }

    async fn content_delete(
        &self,
        scope: &Scope,
        collection: &str,
        doc_id: &str,
        name: &str,
    ) -> Result<bool, StoreError> {
        let key = partition_key(scope, collection);
        let mut conn = self.pool.get().await.map_err(backend)?;
        let tx = conn.transaction().await.map_err(backend)?;
        tx.execute(
            "DELETE FROM content_chunk WHERE partition_key = $1 AND doc_id = $2 AND name = $3",
            &[&key, &doc_id, &name],
        )
        .await
        .map_err(backend)?;
        let removed = tx
            .execute(
                "DELETE FROM content_meta
                 WHERE partition_key = $1 AND doc_id = $2 AND name = $3 AND version IS NOT NULL",
                &[&key, &doc_id, &name],
            )
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(removed > 0)
    }

    async fn scope_documents(&self, scope: &Scope) -> Result<usize, StoreError> {
        let row = self
            .pool
            .get()
            .await
            .map_err(backend)?
            .query_one(
                "SELECT count(*) FROM documents WHERE scope_kind = $1 AND scope_id = $2",
                &[&scope.kind(), &scope.id()],
            )
            .await
            .map_err(backend)?;
        Ok(usize::try_from(row.get::<_, i64>(0)).unwrap_or_default())
    }

    async fn user_scopes(&self) -> Result<std::collections::BTreeMap<String, usize>, StoreError> {
        Ok(self
            .pool
            .get()
            .await
            .map_err(backend)?
            .query(
                "SELECT scope_id, count(*) FROM documents WHERE scope_kind = 'user' GROUP BY scope_id",
                &[],
            )
            .await
            .map_err(backend)?
            .iter()
            .map(|row| {
                (
                    row.get::<_, String>(0),
                    usize::try_from(row.get::<_, i64>(1)).unwrap_or_default(),
                )
            })
            .collect())
    }

    /// One transaction. Rows are inserted under the destination's partition
    /// keys, so each lands in whatever partition the destination's own
    /// descriptor gives it, and its search vector is computed for that
    /// descriptor; then the source's rows, descriptors and partitions go.
    async fn rescope(&self, from: &Scope, to: &Scope) -> Result<super::Rescoped, StoreError> {
        let mut conn = self.pool.get().await.map_err(backend)?;
        let tx = conn.transaction().await.map_err(backend)?;
        let (fk, fi, tk, ti) = (from.kind(), from.id(), to.kind(), to.id());
        let kept: Vec<String> = tx
            .query(
                "SELECT a.collection || '/' || a.doc_id FROM documents a
                 WHERE a.scope_kind = $1 AND a.scope_id = $2
                   AND EXISTS (SELECT 1 FROM documents b
                               WHERE b.partition_key = $3 || '/' || $4 || '/' || a.collection
                                 AND b.doc_id = a.doc_id)
                 ORDER BY 1",
                &[&fk, &fi, &tk, &ti],
            )
            .await
            .map_err(backend)?
            .iter()
            .map(|row| row.get(0))
            .collect();
        let moved = tx
            .execute(
                "INSERT INTO documents (partition_key, doc_id, scope_kind, scope_id, collection,
                                        body, version, created_by, created_at, updated_at,
                                        expires_at, search_vec)
                 SELECT $3 || '/' || $4 || '/' || collection, doc_id, $3, $4, collection,
                        body, version, created_by, created_at, updated_at, expires_at,
                        document_search_vector($3 || '/' || $4 || '/' || collection, body)
                 FROM documents WHERE scope_kind = $1 AND scope_id = $2
                 ON CONFLICT (partition_key, doc_id) DO NOTHING",
                &[&fk, &fi, &tk, &ti],
            )
            .await
            .map_err(backend)?;
        let prefix = format!("{fk}/{fi}/");
        let dest = format!("{tk}/{ti}/");
        for table in ["content_meta", "content_chunk"] {
            tx.execute(
                &format!(
                    "INSERT INTO {table}
                     SELECT (jsonb_populate_record(NULL::{table},
                               to_jsonb(c) || jsonb_build_object('partition_key',
                                   $2 || substr(c.partition_key, length($1) + 1)))).*
                     FROM {table} c
                     WHERE starts_with(c.partition_key, $1)
                       AND NOT EXISTS (SELECT 1 FROM documents b
                                       WHERE b.partition_key = $2 || substr(c.partition_key, length($1) + 1)
                                         AND b.doc_id = c.doc_id
                                         AND (b.collection || '/' || b.doc_id) = ANY($3))
                     ON CONFLICT DO NOTHING"
                ),
                &[&prefix, &dest, &kept],
            )
            .await
            .map_err(backend)?;
        }
        Self::drop_scope_in(&tx, from).await?;
        tx.commit().await.map_err(backend)?;
        Ok(super::Rescoped {
            moved: usize::try_from(moved).unwrap_or_default(),
            kept,
        })
    }

    async fn drop_scope(&self, scope: &Scope) -> Result<usize, StoreError> {
        let mut conn = self.pool.get().await.map_err(backend)?;
        let tx = conn.transaction().await.map_err(backend)?;
        let dropped = Self::drop_scope_in(&tx, scope).await?;
        tx.commit().await.map_err(backend)?;
        Ok(dropped)
    }

    async fn purge_expired(&self) -> Result<u64> {
        let conn = self.pool.get().await?;
        Ok(conn
            .execute(
                "DELETE FROM documents WHERE expires_at IS NOT NULL AND expires_at <= now()",
                &[],
            )
            .await?)
    }

    fn is_shared(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::conformance;
    use workforce_postgres::test_support::with_test_pool;

    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_backend_meets_the_reference_semantics() {
        with_test_pool(build_pool, |pool| async move {
            run_migrations(&pool).await.expect("migrations");
            let store = PostgresDocumentStore::new(pool);
            conformance::run_all(&store).await;
            assert!(store.is_shared());
        })
        .await;
    }

    /// `describe` returning is not evidence the database did anything, so
    /// this asserts against the catalogue: the partition exists, the declared
    /// indexes exist on it, an index the descriptor stops naming is dropped,
    /// and the rows that were already there moved into the new partition.
    #[tokio::test(flavor = "multi_thread")]
    async fn declaring_a_collection_large_builds_its_partition_and_indexes() {
        with_test_pool(build_pool, |pool| async move {
            run_migrations(&pool).await.expect("migrations");
            let store = PostgresDocumentStore::new(pool.clone());
            let scope = Scope::Agent("idx".into());

            for i in 0..5 {
                store
                    .set(
                        &scope,
                        "events",
                        &format!("e{i}"),
                        serde_json::json!({"status": "open", "tags": ["a"]}),
                        None,
                        None,
                        None,
                    )
                    .await
                    .expect("seed");
            }

            let key = partition_key(&scope, "events");
            let table = partition_name(&key);
            let conn = pool.get().await.expect("connection");
            let indexes = |conn: deadpool_postgres::Object, table: String| async move {
                let rows = conn
                    .query(
                        "SELECT indexname FROM pg_indexes
                         WHERE schemaname = 'store' AND tablename = $1
                           AND (indexname LIKE 'docidx\\_%' OR indexname LIKE 'docsearch\\_%')
                         ORDER BY indexname",
                        &[&table],
                    )
                    .await
                    .expect("catalogue");
                rows.iter()
                    .map(|r| r.get::<_, String>(0))
                    .collect::<Vec<_>>()
            };

            assert!(
                indexes(conn, table.clone()).await.is_empty(),
                "an undeclared collection has no indexes of its own"
            );

            let two = Descriptor::from_json(&serde_json::json!({
                "scale": "large",
                "indexes": [{"field": "status"}, {"field": "tags", "type": "contains"}]
            }))
            .unwrap();
            store
                .describe(&scope, "events", &two)
                .await
                .expect("describe");

            let conn = pool.get().await.expect("connection");
            let exists: bool = conn
                .query_one(
                    "SELECT to_regclass($1::text) IS NOT NULL",
                    &[&format!("store.{table}")],
                )
                .await
                .expect("regclass")
                .get(0);
            assert!(exists, "a large collection gets a partition of its own");

            let conn = pool.get().await.expect("connection");
            let built = indexes(conn, table.clone()).await;
            assert_eq!(
                built.len(),
                2,
                "both declared indexes were built: {built:?}"
            );

            let conn = pool.get().await.expect("connection");
            let moved: i64 = conn
                .query_one(&format!("SELECT count(*) FROM store.{table}"), &[])
                .await
                .expect("count")
                .get(0);
            assert_eq!(
                moved, 5,
                "rows already written moved into the new partition"
            );

            let one = Descriptor::from_json(&serde_json::json!({
                "scale": "large",
                "indexes": [{"field": "status"}]
            }))
            .unwrap();
            store
                .describe(&scope, "events", &one)
                .await
                .expect("re-describe");
            let conn = pool.get().await.expect("connection");
            let after = indexes(conn, table.clone()).await;
            assert_eq!(
                after.len(),
                1,
                "an index the descriptor no longer names is dropped: {after:?}"
            );

            // Every document is still readable and writable through the store.
            let page = store
                .query(
                    &scope,
                    "events",
                    &Query::from_json(&serde_json::json!({"where": [["status", "eq", "open"]]}))
                        .unwrap(),
                )
                .await
                .expect("query the partitioned collection");
            assert_eq!(page.documents.len(), 5);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_covers_documents_written_before_it_was_declared() {
        with_test_pool(build_pool, |pool| async move {
            run_migrations(&pool).await.expect("migrations");
            let store = PostgresDocumentStore::new(pool);
            let scope = Scope::Agent("search".into());

            for (id, title, body) in [
                (
                    "a",
                    "Order booking failed",
                    "the worker could not be booked",
                ),
                (
                    "b",
                    "Catalogue published",
                    "a catalogue went out to the team",
                ),
                ("c", "Booking cancelled", "the booking was withdrawn"),
            ] {
                store
                    .set(
                        &scope,
                        "notes",
                        id,
                        serde_json::json!({"title": title, "detail": body}),
                        None,
                        None,
                        None,
                    )
                    .await
                    .expect("seed");
            }

            let search =
                |term: &str| Query::from_json(&serde_json::json!({"search": term})).expect("query");

            assert!(
                store
                    .query(&scope, "notes", &search("booking"))
                    .await
                    .is_err(),
                "a collection that declares no text_search is not quietly scanned"
            );

            store
                .describe(
                    &scope,
                    "notes",
                    &Descriptor::from_json(&serde_json::json!({
                        "text_search": {"fields": ["title", "detail"]}
                    }))
                    .unwrap(),
                )
                .await
                .expect("describe");

            let ids = |page: Page| {
                let mut v: Vec<String> = page.documents.into_iter().map(|d| d.doc_id).collect();
                v.sort();
                v
            };

            assert_eq!(
                ids(store
                    .query(&scope, "notes", &search("booking"))
                    .await
                    .unwrap()),
                ["a", "c"],
                "documents written before the declaration are indexed too"
            );
            assert_eq!(
                ids(store
                    .query(&scope, "notes", &search("booked"))
                    .await
                    .unwrap()),
                ["a", "c"],
                "stemming is why this is tsvector and not a substring match"
            );
            assert_eq!(
                ids(store
                    .query(&scope, "notes", &search("booking -cancelled"))
                    .await
                    .unwrap()),
                ["a"],
                "websearch syntax: an exclusion works the way a person expects"
            );
            assert_eq!(
                ids(store
                    .query(&scope, "notes", &search("\"order booking\""))
                    .await
                    .unwrap()),
                ["a"],
                "a quoted phrase is a phrase"
            );

            // A document written after the declaration is indexed by the write.
            store
                .set(
                    &scope,
                    "notes",
                    "d",
                    serde_json::json!({"title": "Another booking", "detail": "x"}),
                    None,
                    None,
                    None,
                )
                .await
                .expect("write");
            assert_eq!(
                ids(store
                    .query(&scope, "notes", &search("booking"))
                    .await
                    .unwrap()),
                ["a", "c", "d"]
            );

            // Search composes with filters rather than replacing them.
            let combined = Query::from_json(&serde_json::json!({
                "search": "booking",
                "where": [["title", "eq", "Booking cancelled"]]
            }))
            .unwrap();
            assert_eq!(
                ids(store.query(&scope, "notes", &combined).await.unwrap()),
                ["c"]
            );
        })
        .await;
    }

    /// Partitioning rewrites the table every existing document lives in, so
    /// the migration is run here over real pre-partitioning rows rather than
    /// only over the empty table a fresh install starts with.
    #[tokio::test(flavor = "multi_thread")]
    async fn partitioning_carries_every_existing_document_over() {
        with_test_pool(build_pool, |pool| async move {
            let conn = pool.get().await.expect("connection");
            conn.batch_execute("CREATE SCHEMA IF NOT EXISTS store")
                .await
                .expect("schema");
            conn.batch_execute(include_str!("../../migrations_store/V001__documents.sql"))
                .await
                .expect("V001");

            // A scope id and a collection that both contain `:`, the character
            // a naive partition key would have joined on: these two rows must
            // land in different key namespaces, not collide into one.
            let rows = [
                ("agent", "atlas", "pipeline", "ENG-1"),
                ("agent", "a:b", "c", "shared-id"),
                ("agent", "a", "b:c", "shared-id"),
                ("user", "u1", "notes", "n1"),
                ("global", "", "registry", "r1"),
            ];
            for (kind, id, collection, doc) in rows {
                conn.execute(
                    "INSERT INTO documents
                     (scope_kind, scope_id, collection, doc_id, body, version, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, '{\"kept\": true}'::jsonb, 7, now(), now())",
                    &[&kind, &id, &collection, &doc],
                )
                .await
                .expect("seed row");
            }

            conn.batch_execute(include_str!(
                "../../migrations_store/V002__collections_and_partitioning.sql"
            ))
            .await
            .expect("V002");
            conn.batch_execute(include_str!("../../migrations_store/V003__content.sql"))
                .await
                .expect("V003");
            conn.batch_execute(include_str!("../../migrations_store/V004__search.sql"))
                .await
                .expect("V004");

            let kept: i64 = conn
                .query_one("SELECT count(*) FROM documents", &[])
                .await
                .expect("count")
                .get(0);
            assert_eq!(kept, rows.len() as i64, "every document must survive");

            let distinct_keys: i64 = conn
                .query_one("SELECT count(DISTINCT partition_key) FROM documents", &[])
                .await
                .expect("count keys")
                .get(0);
            assert_eq!(
                distinct_keys, 5,
                "`a:b`+`c` and `a`+`b:c` must not share a partition key"
            );

            let row = conn
                .query_one(
                    "SELECT partition_key, body, version FROM documents
                     WHERE scope_kind = 'agent' AND scope_id = 'atlas' AND collection = 'pipeline'",
                    &[],
                )
                .await
                .expect("the migrated row");
            assert_eq!(row.get::<_, String>(0), "agent/atlas/pipeline");
            assert_eq!(row.get::<_, Value>(1), serde_json::json!({"kept": true}));
            assert_eq!(row.get::<_, i64>(2), 7, "version is carried, not reset");

            // Reads and writes work through the store against migrated rows.
            let store = PostgresDocumentStore::new(pool.clone());
            let scope = Scope::Agent("atlas".into());
            let doc = store
                .get(&scope, "pipeline", "ENG-1")
                .await
                .expect("read")
                .expect("present");
            assert_eq!(doc.version, 7);
            let updated = store
                .update(&scope, "pipeline", "ENG-1", serde_json::json!({"n": 1}), Some(7), None)
                .await
                .expect("write against a migrated row");
            assert_eq!(updated.version, 8);
        })
        .await;
    }
}
