//! The hub's journal of bulk mail jobs (schema `mail`): what each
//! `email.move` / `archive` / `mark` / `delete` job did, so it carries on
//! across calls and `email.undo` puts it back. A job is kept two weeks.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Context, Result};
use tokio_postgres::Row;
use tracing::warn;
use workforce_common::settings::WorkforceConfig;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};
use workforce_skills::adapters::email::journal::{self, BulkJob, MailJournal};

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

fn job_of(row: &Row) -> Result<BulkJob> {
    let job: serde_json::Value = row.try_get("job")?;
    serde_json::from_value(job).context("reading a kept bulk mail job")
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

    async fn one(&self, sql: &str, owner: &str, key: &str) -> Result<Option<BulkJob>> {
        let conn = self.pool.get().await?;
        conn.query_opt(sql, &[&owner, &key])
            .await?
            .as_ref()
            .map(job_of)
            .transpose()
    }
}

#[async_trait]
impl MailJournal for PgMailJournal {
    async fn save(&self, job: &BulkJob) -> Result<()> {
        let conn = self.pool.get().await?;
        let kept = serde_json::to_value(job)?;
        conn.execute(
            "INSERT INTO bulk_jobs (id, owner, account, job, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, now()) \
             ON CONFLICT (id) DO UPDATE SET job = EXCLUDED.job, updated_at = now() \
             WHERE bulk_jobs.owner = EXCLUDED.owner",
            &[&job.id, &job.owner, &job.account, &kept, &job.created],
        )
        .await?;
        conn.execute(
            "DELETE FROM bulk_jobs WHERE updated_at < now() - interval '14 days'",
            &[],
        )
        .await?;
        Ok(())
    }

    async fn get(&self, owner: &str, id: &str) -> Result<Option<BulkJob>> {
        self.one(
            "SELECT job FROM bulk_jobs WHERE owner = $1 AND id = $2",
            owner,
            id,
        )
        .await
    }

    async fn latest(&self, owner: &str, account: &str) -> Result<Option<BulkJob>> {
        self.one(
            "SELECT job FROM bulk_jobs WHERE owner = $1 AND account = $2 \
             ORDER BY created_at DESC LIMIT 1",
            owner,
            account,
        )
        .await
    }
}

/// Opens the journal once when the hub has Postgres, and hands it to the
/// email tools.
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
                Ok(store) => journal::install(Arc::new(store)),
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
    use workforce_skills::adapters::email::journal::{BulkAction, JobState};
    use workforce_skills::adapters::email::uids::UidSet;

    use super::*;

    fn job(id: &str, owner: &str, account: &str) -> BulkJob {
        BulkJob {
            id: id.to_string(),
            owner: owner.to_string(),
            account: account.to_string(),
            action: BulkAction::Move {
                from: "INBOX".to_string(),
                to: "News".to_string(),
                label: None,
            },
            asked: "messages from news@shop.com".to_string(),
            validity: 7,
            pending: (1..=300_000).collect::<UidSet>(),
            done: Vec::new(),
            state: JobState::Running,
            stopped: None,
            unfound: 0,
            created: Utc::now(),
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
            journal.save(&stolen).await.unwrap();
            assert_eq!(
                journal.get("kat", "j1").await.unwrap().map(|j| j.state),
                Some(JobState::Done),
                "another person can't overwrite a job"
            );
        })
        .await;
    }
}
