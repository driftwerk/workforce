//! Postgres backend for [`JobStore`]: one row per job in the `scheduler`
//! schema, per-change writes so concurrent hub instances only ever touch the
//! rows they mutated. Pool + migrations are process-wide and memoised; each
//! agent's store scopes queries by `agent_id`.

use async_trait::async_trait;
use chrono::Utc;
use eyre::{Context, Result};
use tokio::sync::OnceCell;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use super::{JobChange, JobStore};
use crate::scheduler::ScheduledJob;

// Own directory, NOT a subdir of `migrations/`: the approvals store embeds
// that whole tree, and refinery's embed is recursive — a shared parent put
// this V001 into the approvals migration set and broke its runner.
mod embedded {
    refinery::embed_migrations!("migrations_scheduler");
}

pub const SCHEMA: &str = "scheduler";

pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
    PostgresPool::from_config_with_session_init(config, &["SET search_path TO scheduler, public"])
}

static SCHEDULER_POOL: OnceCell<PostgresPool> = OnceCell::const_new();

/// The process-wide scheduler pool: built and migrated once, shared by every
/// agent's store. Fails closed — an unreachable Postgres is an error here,
/// never a silent file fall-back.
pub async fn shared_pool(config: &PostgresConfig) -> Result<PostgresPool> {
    SCHEDULER_POOL
        .get_or_try_init(|| async {
            let pool = build_pool(config).context("building scheduler pool")?;
            run_migrations(&pool).await?;
            Ok::<_, eyre::Report>(pool)
        })
        .await
        .cloned()
}

pub async fn run_migrations(pool: &PostgresPool) -> Result<()> {
    let set = MigrationSet::new(
        SCHEMA,
        Box::new(EmbeddedSchedulerMigrations) as Box<dyn MigrationRun>,
    );
    MigrationRunner::run(pool, &[set])
        .await
        .context("running scheduler migrations")
}

struct EmbeddedSchedulerMigrations;

#[async_trait]
impl MigrationRun for EmbeddedSchedulerMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

pub struct PostgresJobStore {
    pool: PostgresPool,
    agent_id: String,
}

impl PostgresJobStore {
    pub fn new(pool: PostgresPool, agent_id: String) -> Self {
        Self { pool, agent_id }
    }

    async fn upsert<C: deadpool_postgres::GenericClient>(
        &self,
        client: &C,
        job: &ScheduledJob,
    ) -> Result<()> {
        let definition = serde_json::to_value(job).context("encode job")?;
        client
            .execute(
                "INSERT INTO jobs (agent_id, id, definition, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (agent_id, id) DO UPDATE SET
                 definition = EXCLUDED.definition,
                 updated_at = EXCLUDED.updated_at",
                &[
                    &self.agent_id,
                    &job.id,
                    &definition,
                    &job.created_at,
                    &Utc::now(),
                ],
            )
            .await?;
        Ok(())
    }

    /// Write `fields` onto the stored row, and nothing when there is no row:
    /// a job a peer removed (an identity's jobs deleted, say) is never brought
    /// back by a write from a copy that predates the removal, such as the
    /// leader's bookkeeping after a run that started before it.
    /// With `while_set`, only while that boolean field of the stored row is
    /// still true.
    async fn update_fields<C: deadpool_postgres::GenericClient>(
        &self,
        client: &C,
        job: &ScheduledJob,
        fields: &[&str],
        while_set: Option<&str>,
    ) -> Result<()> {
        let definition = serde_json::to_value(job).context("encode job")?;
        let written = super::part_of(&definition, fields, true);
        client
            .execute(
                "UPDATE jobs SET definition = definition || $3, updated_at = $4
                 WHERE agent_id = $1 AND id = $2
                   AND ($5::text IS NULL OR definition -> $5::text = 'true'::jsonb)",
                &[&self.agent_id, &job.id, &written, &Utc::now(), &while_set],
            )
            .await?;
        Ok(())
    }

    /// Partial write: a missing row is inserted whole, an existing row takes
    /// `fields` from `job` (or, with `keep` false, everything but them) and
    /// keeps the rest.
    async fn merge<C: deadpool_postgres::GenericClient>(
        &self,
        client: &C,
        job: &ScheduledJob,
        fields: &[&str],
        keep: bool,
    ) -> Result<()> {
        let definition = serde_json::to_value(job).context("encode job")?;
        let written = super::part_of(&definition, fields, keep);
        client
            .execute(
                "INSERT INTO jobs (agent_id, id, definition, created_at, updated_at)
             VALUES ($1, $2, $3, $5, $6)
             ON CONFLICT (agent_id, id) DO UPDATE SET
                 definition = jobs.definition || $4,
                 updated_at = EXCLUDED.updated_at",
                &[
                    &self.agent_id,
                    &job.id,
                    &definition,
                    &written,
                    &job.created_at,
                    &Utc::now(),
                ],
            )
            .await?;
        Ok(())
    }
}

/// The fields of a job's definition that the running scheduler owns; a
/// [`JobChange::Refreshed`] leaves the stored values in place.
const BOOKKEEPING_KEYS: [&str; 5] = [
    "next_run",
    "last_run",
    "run_count",
    "delivery_failures",
    "enabled",
];

#[async_trait]
impl JobStore for PostgresJobStore {
    async fn load_all(&self) -> Result<Vec<ScheduledJob>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT definition FROM jobs WHERE agent_id = $1 ORDER BY created_at, id",
                &[&self.agent_id],
            )
            .await?;
        rows.into_iter()
            .map(|r| serde_json::from_value(r.get(0)).context("decode job"))
            .collect()
    }

    async fn persist(&self, all: &[ScheduledJob], change: JobChange<'_>) -> Result<()> {
        let mut conn = self.pool.get().await?;
        match change {
            JobChange::Upserted(job) => self.upsert(&conn, job).await,
            JobChange::Refreshed(job) => self.merge(&conn, job, &BOOKKEEPING_KEYS, false).await,
            JobChange::Fields(job, fields) => self.update_fields(&conn, job, fields, None).await,
            JobChange::FieldsWhile(job, fields, flag) => {
                self.update_fields(&conn, job, fields, Some(flag)).await
            }
            JobChange::Removed(ids) => {
                conn.execute(
                    "DELETE FROM jobs WHERE agent_id = $1 AND id = ANY($2)",
                    &[&self.agent_id, &ids],
                )
                .await?;
                Ok(())
            }
            JobChange::Replaced => {
                let tx = conn.transaction().await?;
                for job in all {
                    self.upsert(&tx, job).await?;
                }
                tx.commit().await?;
                Ok(())
            }
        }
    }

    fn is_shared(&self) -> bool {
        true
    }
}

/// Backfill one agent's `jobs.json` into Postgres. `ON CONFLICT DO NOTHING`:
/// idempotent, and never clobbers a row the live store has since updated.
/// Returns `(copied, skipped)`.
pub async fn migrate_jobs_file(
    pool: &PostgresPool,
    agent_id: &str,
    jobs_file: &std::path::Path,
) -> Result<(usize, usize)> {
    if !jobs_file.exists() {
        return Ok((0, 0));
    }
    let content = std::fs::read_to_string(jobs_file)
        .with_context(|| format!("reading {}", jobs_file.display()))?;
    let jobs: Vec<ScheduledJob> = serde_json::from_str(&content)
        .with_context(|| format!("parsing {}", jobs_file.display()))?;
    let conn = pool.get().await?;
    let mut copied = 0usize;
    for job in &jobs {
        let definition = serde_json::to_value(job).context("encode job")?;
        let affected = conn
            .execute(
                "INSERT INTO jobs (agent_id, id, definition, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (agent_id, id) DO NOTHING",
                &[
                    &agent_id,
                    &job.id,
                    &definition,
                    &job.created_at,
                    &Utc::now(),
                ],
            )
            .await?;
        copied += affected as usize;
    }
    Ok((copied, jobs.len() - copied))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{JobPayload, Schedule};
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    fn job(id: &str, name: &str) -> ScheduledJob {
        ScheduledJob {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
            schedule: Schedule::Every {
                every_secs: 3600,
                anchor: None,
            },
            payload: JobPayload::SystemEvent {
                text: "test".to_string(),
                channel_id: None,
                thread_id: None,
                user_id: None,
            },
            enabled: true,
            created_at: now_micros(),
            last_run: None,
            next_run: None,
            run_count: 0,
            is_reminder: false,
            delivery_failures: 0,
            creator_identity_id: None,
            run_as_identity_id: None,
            bill_to_identity_id: None,
            visibility: None,
            max_runs: None,
            budget_usd: None,
            expires_at: None,
            deliver_to: None,
            edited_by: None,
            paused_by_suspension: false,
        }
    }

    async fn with_store<F, Fut>(test: F)
    where
        F: FnOnce(PostgresJobStore, PostgresPool) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_test_pool(build_pool, |pool| async move {
            run_migrations(&pool).await.expect("migrations");
            test(PostgresJobStore::new(pool.clone(), "agent-a".into()), pool).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upsert_remove_load_round_trip() {
        with_store(|store, _pool| async move {
            let a = job("a", "one");
            let mut b = job("b", "two");
            store.persist(&[], JobChange::Upserted(&a)).await.unwrap();
            store.persist(&[], JobChange::Upserted(&b)).await.unwrap();
            assert_eq!(store.load_all().await.unwrap().len(), 2);

            b.name = "two-renamed".into();
            store.persist(&[], JobChange::Upserted(&b)).await.unwrap();
            let loaded = store.load_all().await.unwrap();
            assert_eq!(
                loaded.iter().find(|j| j.id == "b").unwrap().name,
                "two-renamed"
            );

            store
                .persist(&[], JobChange::Removed(&["a".to_string()]))
                .await
                .unwrap();
            let left = store.load_all().await.unwrap();
            assert_eq!(left.len(), 1);
            assert_eq!(left[0].id, "b");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refreshed_keeps_the_rows_bookkeeping_and_takes_the_rest() {
        with_store(|store, _pool| async move {
            // The leader fired the 09:52 slot and advanced the row.
            let mut fired = job("wf", "workflow-schedule:pr-review:0");
            fired.next_run = Some(now_micros() + chrono::Duration::minutes(15));
            fired.last_run = Some(now_micros());
            fired.run_count = 7;
            fired.enabled = false;
            store
                .persist(&[], JobChange::Upserted(&fired))
                .await
                .unwrap();

            // A booting follower syncs from its load-time copy: still at the
            // 09:52 occurrence, but with a fresh payload and identity.
            let mut stale = job("wf", "workflow-schedule:pr-review:0");
            stale.next_run = Some(now_micros() - chrono::Duration::seconds(20));
            stale.description = Some("refreshed".into());
            stale.run_as_identity_id = Some("identity-1".into());
            store
                .persist(&[], JobChange::Refreshed(&stale))
                .await
                .unwrap();

            let loaded = store.load_all().await.unwrap();
            let row = loaded.iter().find(|j| j.id == "wf").unwrap();
            assert_eq!(
                row.next_run, fired.next_run,
                "the fired slot stays advanced"
            );
            assert_eq!(row.last_run, fired.last_run);
            assert_eq!(row.run_count, 7);
            assert!(!row.enabled, "a refresh never re-enables");
            assert_eq!(row.description.as_deref(), Some("refreshed"));
            assert_eq!(row.run_as_identity_id.as_deref(), Some("identity-1"));

            // No row yet: the refresh inserts the job whole.
            let fresh = job("new", "workflow-schedule:new:0");
            store
                .persist(&[], JobChange::Refreshed(&fresh))
                .await
                .unwrap();
            let loaded = store.load_all().await.unwrap();
            assert!(loaded.iter().any(|j| j.id == "new" && j.enabled));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fields_write_never_brings_back_a_removed_job() {
        with_store(|store, _pool| async move {
            let fired = job("j", "hourly");
            store
                .persist(&[], JobChange::Upserted(&fired))
                .await
                .unwrap();
            store
                .persist(&[], JobChange::Removed(&["j".to_string()]))
                .await
                .unwrap();
            store
                .persist(&[], JobChange::Fields(&fired, &["last_run", "run_count"]))
                .await
                .unwrap();
            assert!(store.load_all().await.unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fields_write_changes_those_fields_and_keeps_the_rest_of_the_row() {
        with_store(|store, _pool| async move {
            let mut fired = job("j", "hourly");
            fired.last_run = Some(now_micros());
            fired.run_count = 7;
            store
                .persist(&[], JobChange::Upserted(&fired))
                .await
                .unwrap();

            let mut stale = job("j", "renamed");
            stale.visibility = Some("global".into());
            store
                .persist(&[], JobChange::Fields(&stale, &["name"]))
                .await
                .unwrap();

            let loaded = store.load_all().await.unwrap();
            let row = loaded.iter().find(|j| j.id == "j").unwrap();
            assert_eq!(row.name, "renamed");
            assert_eq!(row.visibility, None, "a field not named is not written");
            assert_eq!((row.run_count, row.last_run), (7, fired.last_run));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fields_write_clears_a_suspension_hold_as_well_as_setting_it() {
        with_store(|store, _pool| async move {
            let mut held = job("j", "hourly");
            held.enabled = false;
            held.paused_by_suspension = true;
            store
                .persist(&[], JobChange::Upserted(&held))
                .await
                .unwrap();

            held.enabled = true;
            held.paused_by_suspension = false;
            store
                .persist(
                    &[],
                    JobChange::Fields(&held, &["enabled", "paused_by_suspension"]),
                )
                .await
                .unwrap();

            let row = store.load_all().await.unwrap().remove(0);
            assert!(row.enabled && !row.paused_by_suspension);
        })
        .await;
    }

    /// A release read before a person paused the job by hand elsewhere does
    /// not switch it back on: the write waits on the stored hold.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_guarded_write_lands_only_while_its_flag_is_still_set() {
        with_store(|store, _pool| async move {
            let mut held = job("j", "hourly");
            held.enabled = false;
            held.paused_by_suspension = true;
            store
                .persist(&[], JobChange::Upserted(&held))
                .await
                .unwrap();
            let mut paused_by_hand = held.clone();
            paused_by_hand.paused_by_suspension = false;
            store
                .persist(
                    &[],
                    JobChange::Fields(&paused_by_hand, &["paused_by_suspension"]),
                )
                .await
                .unwrap();

            let mut released = held.clone();
            released.enabled = true;
            released.paused_by_suspension = false;
            let fields = ["enabled", "paused_by_suspension"];
            store
                .persist(
                    &[],
                    JobChange::FieldsWhile(&released, &fields, "paused_by_suspension"),
                )
                .await
                .unwrap();
            assert!(!store.load_all().await.unwrap()[0].enabled);

            store
                .persist(&[], JobChange::Upserted(&held))
                .await
                .unwrap();
            store
                .persist(
                    &[],
                    JobChange::FieldsWhile(&released, &fields, "paused_by_suspension"),
                )
                .await
                .unwrap();
            assert!(store.load_all().await.unwrap()[0].enabled);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replaced_upserts_all_without_deleting_others() {
        with_store(|store, _pool| async move {
            let other = job("kept", "untouched");
            store
                .persist(&[], JobChange::Upserted(&other))
                .await
                .unwrap();
            let bulk = vec![job("x", "ex"), job("y", "why")];
            store.persist(&bulk, JobChange::Replaced).await.unwrap();
            let ids: Vec<String> = store
                .load_all()
                .await
                .unwrap()
                .into_iter()
                .map(|j| j.id)
                .collect();
            assert_eq!(ids.len(), 3, "bulk write never deletes");
            assert!(ids.contains(&"kept".to_string()));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stores_are_scoped_per_agent() {
        with_store(|store, pool| async move {
            store
                .persist(&[], JobChange::Upserted(&job("a", "mine")))
                .await
                .unwrap();
            let theirs = PostgresJobStore::new(pool, "agent-b".into());
            assert!(theirs.load_all().await.unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migrate_jobs_file_is_idempotent_and_never_clobbers() {
        with_store(|store, pool| async move {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("jobs.json");
            let jobs = vec![job("a", "one"), job("b", "two")];
            std::fs::write(&path, serde_json::to_string(&jobs).unwrap()).unwrap();

            let (copied, skipped) = migrate_jobs_file(&pool, "agent-a", &path).await.unwrap();
            assert_eq!((copied, skipped), (2, 0));
            let (copied, skipped) = migrate_jobs_file(&pool, "agent-a", &path).await.unwrap();
            assert_eq!((copied, skipped), (0, 2), "re-run inserts nothing");

            // A live update after the backfill survives a re-run.
            let mut renamed = jobs[0].clone();
            renamed.name = "one-live".into();
            store
                .persist(&[], JobChange::Upserted(&renamed))
                .await
                .unwrap();
            migrate_jobs_file(&pool, "agent-a", &path).await.unwrap();
            let loaded = store.load_all().await.unwrap();
            assert_eq!(
                loaded.iter().find(|j| j.id == "a").unwrap().name,
                "one-live"
            );

            let missing = tmp.path().join("absent.json");
            assert_eq!(
                migrate_jobs_file(&pool, "agent-a", &missing).await.unwrap(),
                (0, 0)
            );
        })
        .await;
    }
}
