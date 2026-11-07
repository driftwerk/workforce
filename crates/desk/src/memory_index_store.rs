//! What session memory's index worker keeps in Postgres: the outbox (closed
//! exchanges not yet in the index), the deletions still to run, and what
//! reconcile and a rebuild compare and reset. Postgres is the record; the
//! index is made again from it whenever it must be.

use std::collections::HashSet;

use eyre::{Context, Result};
use uuid::Uuid;

use crate::memory::passage::{IndexRow, Purge};
use crate::memory::reach::Bound;
use crate::memory_store::queue_purge_of;
use crate::store::DeskStore;

/// An exchange the index should hold: closed, and not past its session's
/// cap. The outbox, reconcile and the cap all read this one predicate.
const BELONGS: &str = "ended IS NOT NULL AND NOT past_cap";

/// The columns an [`IndexRow`] is read from, then its text's version.
const ROW: &str = "id, session, group_id, project, at, ask, answer, steps, compaction, \
                   md5(ask || answer || steps::text)";

/// An outbox row as claimed: what to write, and the version of its text
/// it was read at, so a row whose text changed meanwhile isn't marked
/// written.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub row: IndexRow,
    pub version: String,
}

/// An index deletion still to run.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedPurge {
    pub id: Uuid,
    pub collection: String,
    /// None when what was kept can't be read: it is set aside.
    pub purge: Option<Purge>,
    pub tries: i32,
}

fn pending_of(r: &tokio_postgres::Row) -> Result<Pending> {
    Ok(Pending {
        row: IndexRow {
            id: r.try_get(0)?,
            session: r.try_get(1)?,
            group: r.try_get(2)?,
            project: r.try_get(3)?,
            at: r.try_get(4)?,
            ask: r.try_get(5)?,
            answer: r.try_get(6)?,
            steps: serde_json::from_value(r.try_get(7)?).unwrap_or_default(),
            compaction: r.try_get(8)?,
        },
        version: r.try_get(9)?,
    })
}

impl DeskStore {
    /// The oldest `limit` closed exchanges not yet in the index.
    pub async fn index_pending(&self, limit: i64) -> Result<Vec<Pending>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {ROW} FROM memory_exchanges \
                     WHERE indexed_at IS NULL AND {BELONGS} ORDER BY at, id LIMIT $1"
                ),
                &[&limit],
            )
            .await
            .context("reading the index's outbox")?
            .iter()
            .map(pending_of)
            .collect()
    }

    /// How many closed exchanges aren't in the index yet, of `groups` when
    /// set (`None` in it: ungrouped).
    pub async fn index_backlog(&self, groups: Option<&[Option<String>]>) -> Result<i64> {
        let client = self.pool().get().await?;
        let (any, named, ungrouped) = match groups {
            None => (true, Vec::new(), false),
            Some(g) => (
                false,
                g.iter().flatten().cloned().collect::<Vec<String>>(),
                g.iter().any(Option::is_none),
            ),
        };
        Ok(client
            .query_one(
                &format!(
                    "SELECT COUNT(*) FROM memory_exchanges WHERE indexed_at IS NULL AND {BELONGS} \
                     AND ($1 OR group_id = ANY($2) OR ($3 AND group_id IS NULL))"
                ),
                &[&any, &named, &ungrouped],
            )
            .await?
            .try_get(0)?)
    }

    /// Marks `written` as in the index, each only if its text is still the
    /// version that was written and it still belongs there. How many were.
    pub async fn index_written(&self, written: &[(Uuid, String)]) -> Result<u64> {
        let client = self.pool().get().await?;
        let (ids, versions): (Vec<Uuid>, Vec<String>) = written.iter().cloned().unzip();
        Ok(client
            .execute(
                &format!(
                    "UPDATE memory_exchanges m SET indexed_at = now() \
                     FROM unnest($1::uuid[], $2::text[]) AS v(id, version) \
                     WHERE m.id = v.id AND m.{BELONGS} \
                       AND md5(m.ask || m.answer || m.steps::text) = v.version"
                ),
                &[&ids, &versions],
            )
            .await?)
    }

    /// Queues every exchange of `group` (`None`: ungrouped) to be written
    /// to the index again: its collection was made afresh. How many.
    pub async fn index_requeue_group(&self, group: Option<&str>) -> Result<u64> {
        let client = self.pool().get().await?;
        Ok(client
            .execute(
                "UPDATE memory_exchanges SET indexed_at = NULL \
                 WHERE group_id IS NOT DISTINCT FROM $1 AND indexed_at IS NOT NULL",
                &[&group],
            )
            .await?)
    }

    /// Queues `ids` to be written to the index again.
    pub async fn index_requeue(&self, ids: &[Uuid]) -> Result<u64> {
        let client = self.pool().get().await?;
        Ok(client
            .execute(
                "UPDATE memory_exchanges SET indexed_at = NULL WHERE id = ANY($1)",
                &[&ids],
            )
            .await?)
    }

    /// A page of `group`'s exchanges Postgres says the index holds, by id
    /// after `after`.
    pub async fn index_held_page(
        &self,
        group: Option<&str>,
        after: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<Pending>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT {ROW} FROM memory_exchanges \
                     WHERE group_id IS NOT DISTINCT FROM $1 AND indexed_at IS NOT NULL \
                       AND {BELONGS} AND ($2::uuid IS NULL OR id > $2) \
                     ORDER BY id LIMIT $3"
                ),
                &[&group, &after, &limit],
            )
            .await?
            .iter()
            .map(pending_of)
            .collect()
    }

    /// Which of `ids` are exchanges the index should hold, in `group`.
    pub async fn index_belonging(
        &self,
        group: Option<&str>,
        ids: &[Uuid],
    ) -> Result<HashSet<Uuid>> {
        let client = self.pool().get().await?;
        client
            .query(
                &format!(
                    "SELECT id FROM memory_exchanges WHERE id = ANY($1) \
                     AND group_id IS NOT DISTINCT FROM $2 AND {BELONGS}"
                ),
                &[&ids, &group],
            )
            .await?
            .iter()
            .map(|r| Ok(r.try_get(0)?))
            .collect()
    }

    /// Every group session memory holds exchanges of (`None`: ungrouped).
    pub async fn memory_groups(&self) -> Result<Vec<Option<String>>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT DISTINCT group_id FROM memory_exchanges \
                 UNION SELECT DISTINCT group_id FROM memory_items",
                &[],
            )
            .await?
            .iter()
            .map(|r| Ok(r.try_get(0)?))
            .collect()
    }

    /// The oldest `limit` index deletions not yet done.
    pub async fn purges_pending(&self, limit: i64) -> Result<Vec<QueuedPurge>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT id, collection, filter, tries FROM memory_purges \
                 WHERE done_at IS NULL ORDER BY created_at, id LIMIT $1",
                &[&limit],
            )
            .await?
            .iter()
            .map(|r| {
                Ok(QueuedPurge {
                    id: r.try_get(0)?,
                    collection: r.try_get(1)?,
                    purge: serde_json::from_value(r.try_get(2)?).ok(),
                    tries: r.try_get(3)?,
                })
            })
            .collect()
    }

    /// How many index deletions are still to run.
    pub async fn purges_backlog(&self) -> Result<i64> {
        let client = self.pool().get().await?;
        Ok(client
            .query_one(
                "SELECT COUNT(*) FROM memory_purges WHERE done_at IS NULL",
                &[],
            )
            .await?
            .try_get(0)?)
    }

    pub async fn purge_done(&self, id: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE memory_purges SET done_at = now() WHERE id = $1",
                &[&id],
            )
            .await?;
        Ok(())
    }

    pub async fn purge_tried(&self, id: Uuid) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "UPDATE memory_purges SET tries = tries + 1 WHERE id = $1",
                &[&id],
            )
            .await?;
        Ok(())
    }

    /// Queues deletions outside a forget or a sweep: reconcile's orphans.
    pub async fn queue_purges(&self, collection: &str, purges: &[Purge]) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        for p in purges {
            queue_purge_of(&tx, collection, p).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Starts the index again from Postgres, in one transaction: each of
    /// `collections` is queued to be dropped, and every exchange to be
    /// written again after.
    pub async fn rebuild_index(&self, collections: &[String]) -> Result<u64> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        // A deletion queued before is overtaken by the drop.
        tx.execute(
            "UPDATE memory_purges SET done_at = now() \
             WHERE done_at IS NULL AND collection = ANY($1)",
            &[&collections],
        )
        .await?;
        for c in collections {
            queue_purge_of(&tx, c, &Purge::All).await?;
        }
        let queued = tx
            .execute(
                "UPDATE memory_exchanges SET indexed_at = NULL WHERE indexed_at IS NOT NULL",
                &[],
            )
            .await?;
        tx.commit().await?;
        Ok(queued)
    }

    /// How much disk session memory's tables take in Postgres, in bytes.
    pub async fn memory_disk_bytes(&self) -> Result<u64> {
        let client = self.pool().get().await?;
        let bytes: i64 = client
            .query_one(
                "SELECT COALESCE(SUM(pg_total_relation_size(c.oid)), 0)::int8 FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind = 'r' AND n.nspname = current_schema() \
                   AND c.relname LIKE 'memory\\_%'",
                &[],
            )
            .await?
            .try_get(0)?;
        Ok(u64::try_from(bytes).unwrap_or(0))
    }
}

/// An exchange a recall found, read back from Postgres by its ref.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallRow {
    pub id: Uuid,
    pub session: Uuid,
    pub group: Option<String>,
    pub project: String,
    pub at: chrono::DateTime<chrono::Utc>,
    pub asker: crate::memory::record::Asker,
    pub ask: String,
    pub answer: String,
    pub compaction: bool,
    /// It has tool steps.
    pub stepped: bool,
    pub flagged: bool,
}

/// [`Bound`]'s parts as SQL parameters: the groups named, whether
/// ungrouped is among them, and the sessions and projects it keeps to.
fn bound_params(b: &Bound) -> (Vec<String>, bool, Option<Vec<Uuid>>, Option<Vec<String>>) {
    (
        b.groups.iter().flatten().cloned().collect(),
        b.groups.iter().any(Option::is_none),
        b.sessions.clone(),
        b.projects.clone(),
    )
}

/// The SQL of [`Bound`] over `memory_exchanges`, parameters `$first` on.
fn bound_sql(first: usize) -> String {
    let (g, u, s, p) = (first, first + 1, first + 2, first + 3);
    format!(
        "(group_id = ANY(${g}) OR (${u} AND group_id IS NULL)) \
         AND (${s}::uuid[] IS NULL OR session = ANY(${s})) \
         AND (${p}::text[] IS NULL OR project = ANY(${p}))"
    )
}

impl DeskStore {
    /// The exchanges `ids` names that still exist.
    pub async fn recall_rows(&self, ids: &[Uuid]) -> Result<Vec<RecallRow>> {
        let client = self.pool().get().await?;
        client
            .query(
                "SELECT id, session, group_id, project, at, asker, ask, answer, compaction, \
                   jsonb_array_length(steps) > 0, cardinality(flagged) > 0 \
                 FROM memory_exchanges WHERE id = ANY($1)",
                &[&ids],
            )
            .await
            .context("reading recalled exchanges")?
            .iter()
            .map(|r| {
                Ok(RecallRow {
                    id: r.try_get(0)?,
                    session: r.try_get(1)?,
                    group: r.try_get(2)?,
                    project: r.try_get(3)?,
                    at: r.try_get(4)?,
                    asker: crate::memory::record::Asker::parse(r.try_get(5)?)?,
                    ask: r.try_get(6)?,
                    answer: r.try_get(7)?,
                    compaction: r.try_get(8)?,
                    stepped: r.try_get(9)?,
                    flagged: r.try_get(10)?,
                })
            })
            .collect()
    }

    /// The exchanges within `bound` whose words match `q`, best first, each
    /// with its rank from 0 to 1: every one, or (`unindexed`) only those
    /// the search index doesn't hold yet, or ever will (past the cap).
    pub async fn memory_keyword(
        &self,
        q: &str,
        bound: &Bound,
        unindexed: bool,
        limit: i64,
    ) -> Result<Vec<(Uuid, f32)>> {
        let client = self.pool().get().await?;
        let (named, ungrouped, sessions, projects) = bound_params(bound);
        client
            .query(
                &format!(
                    "SELECT id, ts_rank_cd(tsv, query, 32) FROM memory_exchanges, \
                       websearch_to_tsquery('simple', $1) query \
                     WHERE tsv @@ query AND (NOT $2 OR indexed_at IS NULL OR past_cap) AND {} \
                     ORDER BY ts_rank_cd(tsv, query, 32) DESC, at DESC LIMIT $3",
                    bound_sql(4)
                ),
                &[
                    &q, &unindexed, &limit, &named, &ungrouped, &sessions, &projects,
                ],
            )
            .await
            .context("searching session memory by keyword")?
            .iter()
            .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
            .collect()
    }

    /// Keeps who read what: `hits` found by `reader` with `tool`.
    pub async fn memory_read(
        &self,
        reader: &str,
        tool: &str,
        query: Option<&str>,
        hits: &[Uuid],
        group: Option<&str>,
    ) -> Result<()> {
        let client = self.pool().get().await?;
        client
            .execute(
                "INSERT INTO memory_reads (id, at, reader, tool, query, hits, group_id) \
                 VALUES (gen_random_uuid(), now(), $1, $2, $3, $4, $5)",
                &[&reader, &tool, &query, &hits, &group],
            )
            .await
            .context("noting a read of session memory")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::ledger_store::Cursor;
    use crate::memory::record::{Asker, Exchange};
    use crate::memory_store::{RecordWrite, SESSION_CAP};
    use crate::store::{build_pool, tests::record_fixture};
    use chrono::Utc;
    use workforce_postgres::test_support::with_test_pool;

    fn exchange(key: &str, answer: &str) -> Exchange {
        Exchange {
            key: key.into(),
            at: Utc::now(),
            ended: Some(Utc::now()),
            asker: Asker::Person,
            ask: "fix the cookie flag".into(),
            answer: answer.into(),
            steps: Vec::new(),
            files: Default::default(),
            commands: Vec::new(),
            errors: Vec::new(),
            compaction: false,
            redacted: 0,
            flagged: Default::default(),
        }
    }

    async fn write(store: &DeskStore, session: Uuid, x: &Exchange) {
        store
            .record_write(&RecordWrite {
                session,
                engine: Engine::Claude,
                source: "/records/t.jsonl",
                cursor: &Cursor::default(),
                rows: &[],
                closed: std::slice::from_ref(x),
                open: None,
                project: "/dev/acme-web",
                folder: "/dev/acme-web",
                name: "acme-web",
                cli_session_id: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_exchange_whose_text_changed_is_written_again_and_one_read_before_isnt_marked() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let s = record_fixture("acme-web");
            store.put(&s).await.unwrap();
            write(&store, s.id, &exchange("x-1", "done")).await;
            let read = store.index_pending(50).await.unwrap();
            assert_eq!(read.len(), 1);
            let marked = |p: &Pending| (p.row.id, p.version.clone());
            assert_eq!(store.index_written(&[marked(&read[0])]).await.unwrap(), 1);
            assert!(store.index_pending(50).await.unwrap().is_empty());

            // Read again unchanged: still written.
            write(&store, s.id, &exchange("x-1", "done")).await;
            assert!(store.index_pending(50).await.unwrap().is_empty());
            // Its text changed: written again; what was read before it
            // changed is never marked written.
            write(&store, s.id, &exchange("x-1", "done, and the test passes")).await;
            let again = store.index_pending(50).await.unwrap();
            assert_eq!(again.len(), 1);
            assert_eq!(store.index_written(&[marked(&read[0])]).await.unwrap(), 0);
            assert_eq!(store.index_written(&[marked(&again[0])]).await.unwrap(), 1);
        })
        .await;
    }

    #[tokio::test]
    async fn a_session_past_its_cap_keeps_its_oldest_exchanges_out_of_the_index() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let (big, small) = (record_fixture("acme-big"), record_fixture("acme-small"));
            for s in [&big, &small] {
                store.put(s).await.unwrap();
            }
            let over = 3;
            {
                let client = store.pool().get().await.unwrap();
                let n = i32::try_from(SESSION_CAP).unwrap() + over;
                client
                    .execute(
                        "INSERT INTO memory_exchanges (id, engine, key, session, project, folder, \
                           seq, at, ended, asker, ask, answer, steps, indexed_at) \
                         SELECT gen_random_uuid(), 'claude', 'k-' || i, $1, '/dev/acme', '/dev/acme', \
                           i, now(), now(), 'person', 'ask ' || i, 'answer', '[]', \
                           CASE WHEN i <= 2 THEN now() END \
                         FROM generate_series(1, $2) i",
                        &[&big.id, &n],
                    )
                    .await
                    .unwrap();
            }
            write(&store, small.id, &exchange("x-small", "done")).await;
            let swept = store.sweep_memory(180).await.unwrap();
            assert_eq!((swept.gone, swept.capped, swept.purges), (0, 3, 1));
            let client = store.pool().get().await.unwrap();
            let capped: Vec<i32> = client
                .query(
                    "SELECT seq FROM memory_exchanges WHERE past_cap ORDER BY seq",
                    &[],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| r.get(0))
                .collect();
            assert_eq!(capped, [1, 2, 3], "the oldest go first");
            let purge: serde_json::Value = client
                .query_one("SELECT filter FROM memory_purges", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(purge["of"], "refs");
            assert_eq!(purge["refs"].as_array().unwrap().len(), 3);
            // Never written, never marked, and never searched for by meaning.
            let pending = store.index_pending(100_000).await.unwrap();
            assert!(pending.iter().all(|p| !["ask 1", "ask 2", "ask 3"].contains(&p.row.ask.as_str())));
            assert_eq!(
                store.index_backlog(None).await.unwrap(),
                i64::try_from(SESSION_CAP).unwrap() + 1,
                "the big session's newest, and the small one's"
            );
            let (_, counts) = store.session_memory(big.id, 1).await.unwrap();
            assert_eq!(counts.past_cap, 3);
            assert_eq!(store.sweep_memory(180).await.unwrap().capped, 0, "once");
        })
        .await;
    }

    #[tokio::test]
    async fn a_rebuild_queues_every_collection_dropped_and_every_exchange_again() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let s = record_fixture("acme-web");
            store.put(&s).await.unwrap();
            write(&store, s.id, &exchange("x-1", "done")).await;
            let read = store.index_pending(50).await.unwrap();
            store
                .index_written(&[(read[0].row.id, read[0].version.clone())])
                .await
                .unwrap();
            store
                .queue_purges("DeskWork_u", &[Purge::Session { session: s.id }])
                .await
                .unwrap();
            let queued = store
                .rebuild_index(&["DeskWork_u".to_string()])
                .await
                .unwrap();
            assert_eq!(queued, 1);
            let left = store.purges_pending(10).await.unwrap();
            assert_eq!(
                left.len(),
                1,
                "the earlier deletion is overtaken by the drop"
            );
            assert_eq!(left[0].purge, Some(Purge::All));
            assert_eq!(store.index_backlog(None).await.unwrap(), 1);
            assert_eq!(store.memory_groups().await.unwrap(), [None]);
            assert!(store.memory_disk_bytes().await.unwrap() > 0);
        })
        .await;
    }
}
