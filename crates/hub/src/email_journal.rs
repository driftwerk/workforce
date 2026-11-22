//! The hub's journal of bulk mail jobs (schema `mail`): what each
//! `email.move` / `archive` / `mark` / `delete` job did, so it carries on
//! across calls and `email.undo` puts it back. A job is a row and each batch
//! it did a row of its own, so a batch writes only itself. A job is kept two
//! weeks after it last changed.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{bail, ensure, Context, ContextCompat, Result};
use serde::Serialize;
use serde_json::Value;
use tracing::warn;
use workforce_common::settings::WorkforceConfig;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};
use workforce_skills::adapters::email::journal::{self, BulkJob, JobState, MailJournal, Origin};

static INSTALLED: std::sync::OnceLock<Arc<PgMailJournal>> = std::sync::OnceLock::new();

/// The hub's journal, once [`install`] opened it.
pub fn installed() -> Option<Arc<PgMailJournal>> {
    INSTALLED.get().cloned()
}

/// How often jobs past their two weeks are let go.
const PRUNE_EVERY: Duration = Duration::from_secs(6 * 3600);

// Own directory, never a subdir of another embed root: refinery's
// embed_migrations is recursive.
mod embedded {
    refinery::embed_migrations!("migrations_mail");
}

struct Migrations;

#[async_trait]
impl MigrationRun for Migrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        workforce_postgres::run_embedded(embedded::migrations::runner(), client).await?;
        Ok(())
    }
}

pub struct PgMailJournal {
    pool: PostgresPool,
}

/// A job field as the column that keeps it: its serde name.
fn word(value: impl Serialize) -> Result<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_string)
        .context("a job field that isn't a word")
}

impl PgMailJournal {
    pub const SCHEMA: &'static str = "mail";

    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(
            &config.small(),
            &["SET search_path TO mail, public"],
        )
    }

    pub async fn with_pool(pool: PostgresPool) -> Result<Self> {
        MigrationRunner::run(
            &pool,
            &[MigrationSet::new(
                Self::SCHEMA,
                Box::new(Migrations) as Box<dyn MigrationRun>,
            )],
        )
        .await
        .context("running the mail journal migrations")?;
        Ok(Self { pool })
    }

    /// Lets go of the jobs that haven't changed in two weeks, batches and
    /// all.
    pub async fn prune(&self) -> Result<u64> {
        let conn = self.pool.get().await?;
        Ok(conn
            .execute(
                "DELETE FROM bulk_jobs WHERE updated_at < now() - interval '14 days'",
                &[],
            )
            .await?)
    }

    /// Everyone with jobs kept, with how many each.
    pub async fn people(&self) -> Result<Vec<(String, usize)>> {
        let conn = self.pool.get().await?;
        Ok(conn
            .query(
                "SELECT owner, count(*) FROM bulk_jobs WHERE owner <> '' GROUP BY owner",
                &[],
            )
            .await?
            .iter()
            .map(|row| {
                let held: i64 = row.get(1);
                (row.get(0), usize::try_from(held).unwrap_or(usize::MAX))
            })
            .collect())
    }

    /// How many jobs are kept under any of `names`.
    pub async fn held(&self, names: &[String]) -> Result<usize> {
        let conn = self.pool.get().await?;
        let held: i64 = conn
            .query_one(
                "SELECT count(*) FROM bulk_jobs WHERE owner = ANY($1)",
                &[&names],
            )
            .await?
            .get(0);
        Ok(usize::try_from(held).unwrap_or(usize::MAX))
    }

    /// Hands the jobs kept under any of `names` to `into`, so they can
    /// still be carried on and undone.
    pub async fn reassign(&self, names: &[String], into: &str) -> Result<u64> {
        let conn = self.pool.get().await?;
        Ok(conn
            .execute(
                "UPDATE bulk_jobs SET owner = $2, job = jsonb_set(job, '{owner}', to_jsonb($2::text)) \
                 WHERE owner = ANY($1)",
                &[&names, &into],
            )
            .await?)
    }

    /// Erases the jobs kept under any of `names`, batches and all.
    pub async fn erase(&self, names: &[String]) -> Result<u64> {
        let conn = self.pool.get().await?;
        Ok(conn
            .execute("DELETE FROM bulk_jobs WHERE owner = ANY($1)", &[&names])
            .await?)
    }

    /// The job kept under `sql`'s one row, its batches put back in order.
    async fn one(
        &self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Option<BulkJob>> {
        let conn = self.pool.get().await?;
        let Some(row) = conn.query_opt(sql, params).await? else {
            return Ok(None);
        };
        let id: String = row.try_get("id")?;
        let mut head: Value = row.try_get("job")?;
        let done = conn
            .query(
                "SELECT batch FROM bulk_batches WHERE job_id = $1 ORDER BY seq",
                &[&id],
            )
            .await?
            .iter()
            .map(|row| row.try_get::<_, Value>("batch"))
            .collect::<Result<Vec<_>, _>>()?;
        head.as_object_mut()
            .context("a kept bulk mail job that isn't an object")?
            .insert("done".to_string(), Value::Array(done));
        serde_json::from_value(head)
            .map(Some)
            .context("reading a kept bulk mail job")
    }
}

#[async_trait]
impl MailJournal for PgMailJournal {
    async fn save(&self, job: &BulkJob) -> Result<()> {
        let mut head = serde_json::to_value(job)?;
        head.as_object_mut()
            .context("a bulk mail job that isn't an object")?
            .remove("done");
        ensure!(!job.owner.is_empty(), "a mail job needs an owner");
        let (origin, state) = (word(job.origin)?, word(job.state)?);
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let kept = tx
            .execute(
                "INSERT INTO bulk_jobs (id, owner, account, origin, state, job, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, now()) \
                 ON CONFLICT (id) DO UPDATE SET state = EXCLUDED.state, job = EXCLUDED.job, updated_at = now() \
                 WHERE bulk_jobs.owner = EXCLUDED.owner",
                &[&job.id, &job.owner, &job.account, &origin, &state, &head, &job.created],
            )
            .await?;
        if kept == 0 {
            bail!("job {} is another person's", job.id);
        }
        let stored: i32 = tx
            .query_one(
                "SELECT COALESCE(MAX(seq) + 1, 0) FROM bulk_batches WHERE job_id = $1",
                &[&job.id],
            )
            .await?
            .try_get(0)?;
        let len = i32::try_from(job.done.len())?;
        tx.execute(
            "DELETE FROM bulk_batches WHERE job_id = $1 AND seq >= $2",
            &[&job.id, &len],
        )
        .await?;
        for (seq, batch) in job.done.iter().enumerate().skip(stored.max(0) as usize) {
            tx.execute(
                "INSERT INTO bulk_batches (job_id, seq, batch) VALUES ($1, $2, $3)",
                &[&job.id, &i32::try_from(seq)?, &serde_json::to_value(batch)?],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn get(&self, owner: &str, id: &str) -> Result<Option<BulkJob>> {
        self.one(
            "SELECT id, job FROM bulk_jobs WHERE owner = $1 AND id = $2",
            &[&owner, &id],
        )
        .await
    }

    async fn latest(&self, owner: &str, account: &str) -> Result<Option<BulkJob>> {
        let person = word(Origin::Person)?;
        let settled = JobState::SETTLED
            .into_iter()
            .map(word)
            .collect::<Result<Vec<_>>>()?;
        self.one(
            "SELECT id, job FROM bulk_jobs \
             WHERE owner = $1 AND account = $2 AND origin = $3 AND state <> ALL($4) \
             ORDER BY created_at DESC LIMIT 1",
            &[&owner, &account, &person, &settled],
        )
        .await
    }
}

/// Opens the journal once when the hub has Postgres, hands it to the email
/// tools, and lets go of old jobs from then on.
pub async fn install(config: &WorkforceConfig) {
    static OPENED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    OPENED
        .get_or_init(|| async {
            let Some(pg) = config.postgres.as_ref().filter(|p| p.enabled) else {
                return;
            };
            let opened = match PgMailJournal::build_pool(pg) {
                Ok(pool) => PgMailJournal::with_pool(pool).await,
                Err(e) => Err(e.into()),
            };
            match opened {
                Ok(store) => {
                    let store = Arc::new(store);
                    let _ = INSTALLED.set(Arc::clone(&store));
                    journal::install(Arc::clone(&store) as Arc<dyn MailJournal>);
                    tokio::spawn(async move {
                        let mut every = tokio::time::interval(PRUNE_EVERY);
                        loop {
                            every.tick().await;
                            if let Err(e) = store.prune().await {
                                warn!(error = %format!("{e:#}"), "pruning the mail journal failed");
                            }
                        }
                    });
                }
                Err(e) => warn!(
                    error = %format!("{e:#}"),
                    "mail journal unavailable; email actions by search are refused and nothing can be undone"
                ),
            }
        })
        .await;
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use workforce_postgres::test_support::with_test_pool;
    use workforce_skills::adapters::email::journal::{Batch, BulkAction};
    use workforce_skills::adapters::email::uids::UidSet;

    use super::*;

    fn job(id: &str, owner: &str, account: &str) -> BulkJob {
        BulkJob {
            id: id.to_string(),
            owner: owner.to_string(),
            account: account.to_string(),
            tool: "email.move".to_string(),
            origin: Origin::Person,
            action: BulkAction::Move {
                from: "INBOX".to_string(),
                to: "News".to_string(),
                label: None,
            },
            asked: "messages from news@shop.com".to_string(),
            validity: Some(7),
            pending: (1..=300_000).collect::<UidSet>(),
            in_flight: None,
            done: Vec::new(),
            state: JobState::Running,
            stopped: None,
            unfound: 0,
            created: Utc::now(),
        }
    }

    fn batch(from: u32) -> Batch {
        Batch {
            src: (from..from + 500).collect(),
            dst: None,
            dst_validity: None,
            dst_next: None,
            message_ids: (from..from + 500)
                .map(|uid| (uid, format!("<{uid}@shop.com>")))
                .collect(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_job_is_kept_as_it_goes_and_read_back_only_by_its_owner() {
        with_test_pool(PgMailJournal::build_pool, |pool| async move {
            let journal = PgMailJournal::with_pool(pool).await.unwrap();
            let mut first = job("j1", "kat", "home");
            journal.save(&first).await.unwrap();
            first.pending = UidSet::parse("501:300000").unwrap();
            first.state = JobState::Done;
            journal.save(&first).await.unwrap();
            assert_eq!(journal.get("kat", "j1").await.unwrap(), Some(first.clone()));
            assert_eq!(journal.get("bob", "j1").await.unwrap(), None);

            let mut second = job("j2", "kat", "home");
            second.created = first.created + chrono::Duration::seconds(5);
            journal.save(&second).await.unwrap();
            journal.save(&job("j3", "kat", "work")).await.unwrap();
            assert_eq!(
                journal.latest("kat", "home").await.unwrap().map(|j| j.id),
                Some("j2".to_string())
            );
            assert_eq!(journal.latest("bob", "home").await.unwrap(), None);

            let mut stolen = job("j1", "bob", "home");
            stolen.state = JobState::Undone;
            stolen.done.push(batch(1));
            assert!(
                journal.save(&stolen).await.is_err(),
                "a save under another owner fails, so the job stops"
            );
            assert!(journal.save(&job("j9", "", "home")).await.is_err());
            assert!(journal
                .people()
                .await
                .unwrap()
                .iter()
                .all(|(p, _)| !p.is_empty()));
            let kept = journal.get("kat", "j1").await.unwrap().unwrap();
            assert_eq!(
                kept.state,
                JobState::Done,
                "another person can't overwrite a job"
            );
            assert!(kept.done.is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn batches_are_kept_one_row_each_and_undo_takes_them_off() {
        with_test_pool(PgMailJournal::build_pool, |pool| async move {
            let journal = PgMailJournal::with_pool(pool).await.unwrap();
            let mut moving = job("j1", "kat", "home");
            moving.in_flight = Some(batch(1));
            journal.save(&moving).await.unwrap();
            for from in [1, 501, 1001] {
                moving.in_flight = None;
                moving.done.push(batch(from));
                journal.save(&moving).await.unwrap();
            }
            assert_eq!(
                journal.get("kat", "j1").await.unwrap(),
                Some(moving.clone())
            );
            let rows: i64 = journal
                .pool
                .get()
                .await
                .unwrap()
                .query_one("SELECT count(*) FROM bulk_batches WHERE job_id = 'j1'", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(rows, 3);
            moving.state = JobState::Undoing;
            moving.done.pop();
            journal.save(&moving).await.unwrap();
            assert_eq!(journal.get("kat", "j1").await.unwrap(), Some(moving));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_latest_to_undo_is_the_persons_own_and_not_yet_undone() {
        with_test_pool(PgMailJournal::build_pool, |pool| async move {
            let journal = PgMailJournal::with_pool(pool).await.unwrap();
            let base = Utc::now();
            let mut mine = job("mine", "kat", "home");
            mine.created = base;
            let mut sorted = job("sorted", "kat", "home");
            sorted.origin = Origin::Automation;
            sorted.created = base + chrono::Duration::seconds(5);
            let mut undone = job("undone", "kat", "home");
            undone.state = JobState::Undone;
            undone.created = base + chrono::Duration::seconds(10);
            let mut past = job("past", "kat", "home");
            past.state = JobState::Irreversible;
            past.created = base + chrono::Duration::seconds(15);
            for j in [&mine, &sorted, &undone, &past] {
                journal.save(j).await.unwrap();
            }
            assert_eq!(
                journal.latest("kat", "home").await.unwrap().map(|j| j.id),
                Some("mine".to_string())
            );
        })
        .await;
    }

    /// A person's jobs, by every name they go by, are counted, handed to
    /// the one they are merged into and erased with them.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_persons_jobs_follow_a_merge_and_go_with_a_delete() {
        with_test_pool(PgMailJournal::build_pool, |pool| async move {
            let journal = PgMailJournal::with_pool(pool).await.unwrap();
            let mut moved = job("j1", "u1", "home");
            moved.done.push(batch(1));
            for j in [
                &moved,
                &job("j2", "slack:U1", "home"),
                &job("j3", "u2", "home"),
            ] {
                journal.save(j).await.unwrap();
            }
            let u1 = ["u1".to_string(), "slack:U1".to_string()];
            let mut people = journal.people().await.unwrap();
            people.sort();
            assert_eq!(
                people,
                vec![
                    ("slack:U1".to_string(), 1),
                    ("u1".to_string(), 1),
                    ("u2".to_string(), 1)
                ]
            );
            assert_eq!(journal.held(&u1).await.unwrap(), 2);

            assert_eq!(journal.reassign(&u1, "u2").await.unwrap(), 2);
            assert_eq!(journal.held(&u1).await.unwrap(), 0);
            let merged = journal.get("u2", "j1").await.unwrap().unwrap();
            assert_eq!(merged.owner, "u2");
            assert_eq!(merged.done.len(), 1, "a merged job can still be undone");
            let mut undoing = merged;
            undoing.state = JobState::Undoing;
            journal.save(&undoing).await.unwrap();
            assert_eq!(
                journal.get("u2", "j1").await.unwrap().map(|j| j.state),
                Some(JobState::Undoing),
                "and its new owner can carry it on"
            );

            let u2 = ["u2".to_string()];
            assert_eq!(journal.erase(&u2).await.unwrap(), 3);
            assert!(journal.people().await.unwrap().is_empty());
            let batches: i64 = journal
                .pool
                .get()
                .await
                .unwrap()
                .query_one("SELECT count(*) FROM bulk_batches", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(batches, 0, "its batches go with it");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn jobs_untouched_for_two_weeks_are_let_go() {
        with_test_pool(PgMailJournal::build_pool, |pool| async move {
            let journal = PgMailJournal::with_pool(pool).await.unwrap();
            let mut old = job("old", "kat", "home");
            old.done.push(batch(1));
            journal.save(&old).await.unwrap();
            journal.save(&job("new", "kat", "home")).await.unwrap();
            journal
                .pool
                .get()
                .await
                .unwrap()
                .execute(
                    "UPDATE bulk_jobs SET updated_at = now() - interval '15 days' WHERE id = 'old'",
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(journal.prune().await.unwrap(), 1);
            assert_eq!(journal.get("kat", "old").await.unwrap(), None);
            assert!(journal.get("kat", "new").await.unwrap().is_some());
        })
        .await;
    }
}
