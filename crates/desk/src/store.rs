//! The session registry in Postgres (`desk` schema). One row per session;
//! every change is a single-row upsert so the registry is always what the
//! hub last knew, which is what restore after a restart reads.

use async_trait::async_trait;
use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

use crate::model::{Approvals, Engine, Origin, SessionRecord, SessionState};

mod embedded {
    refinery::embed_migrations!("migrations");
}

pub const SCHEMA: &str = "desk";

pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
    PostgresPool::from_config_with_session_init(
        &config.small(),
        &["SET search_path TO desk, public"],
    )
}

pub struct DeskStore {
    pool: PostgresPool,
}

impl DeskStore {
    /// Builds the pool and applies the schema's migrations. Fails closed: the
    /// registry has no file fall-back.
    pub async fn connect(config: &PostgresConfig) -> Result<Self> {
        let pool = build_pool(config).context("building desk pool")?;
        Self::migrate(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn migrate(pool: &PostgresPool) -> Result<()> {
        let set = MigrationSet::new(SCHEMA, Box::new(Embedded) as Box<dyn MigrationRun>);
        MigrationRunner::run(pool, &[set])
            .await
            .context("running desk migrations")
    }

    /// A store on `pool`, its migrations applied.
    pub async fn with_pool(pool: PostgresPool) -> Result<Self> {
        Self::migrate(&pool).await?;
        Ok(Self { pool })
    }

    pub(crate) fn pool(&self) -> &PostgresPool {
        &self.pool
    }

    #[cfg(test)]
    pub(crate) fn from_pool(pool: PostgresPool) -> Self {
        Self { pool }
    }

    pub async fn all(&self) -> Result<Vec<SessionRecord>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT * FROM sessions WHERE state <> 'closed' ORDER BY updated_at DESC",
                &[],
            )
            .await?;
        rows.iter().map(record).collect()
    }

    pub async fn get(&self, id: Uuid) -> Result<Option<SessionRecord>> {
        let client = self.pool.get().await?;
        client
            .query_opt("SELECT * FROM sessions WHERE id = $1", &[&id])
            .await?
            .as_ref()
            .map(record)
            .transpose()
    }

    pub async fn by_cli_session(
        &self,
        engine: Engine,
        cli_session_id: &str,
    ) -> Result<Option<SessionRecord>> {
        let client = self.pool.get().await?;
        client
            .query_opt(
                "SELECT * FROM sessions WHERE engine = $1 AND cli_session_id = $2",
                &[&engine.as_str(), &cli_session_id],
            )
            .await?
            .as_ref()
            .map(record)
            .transpose()
    }

    pub async fn put(&self, r: &SessionRecord) -> Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                "INSERT INTO sessions (id, engine, name, goal, cwd, branch, origin, state, \
                 approvals, cli_session_id, transcript, last_prompt, last_reply, external_pid, \
                 restore_offer, created_at, updated_at, terminal_id, display_name) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19) \
                 ON CONFLICT (id) DO UPDATE SET engine = $2, name = $3, goal = $4, cwd = $5, \
                 branch = $6, origin = $7, state = $8, approvals = $9, cli_session_id = $10, \
                 transcript = $11, last_prompt = $12, last_reply = $13, external_pid = $14, \
                 restore_offer = $15, updated_at = $17, terminal_id = $18, display_name = $19",
                &[
                    &r.id,
                    &r.engine.as_str(),
                    &r.name,
                    &r.goal,
                    &r.cwd,
                    &r.branch,
                    &r.origin.as_str(),
                    &r.state.as_str(),
                    &r.approvals.as_str(),
                    &r.cli_session_id,
                    &r.transcript,
                    &r.last_prompt,
                    &r.last_reply,
                    &r.external_pid,
                    &r.restore_offer,
                    &r.created_at,
                    &r.updated_at,
                    &r.terminal_id,
                    &r.display_name,
                ],
            )
            .await
            .context("saving desk session")?;
        Ok(())
    }

    pub async fn delete(&self, id: Uuid) -> Result<()> {
        let client = self.pool.get().await?;
        client
            .execute("DELETE FROM sessions WHERE id = $1", &[&id])
            .await?;
        Ok(())
    }
}

fn record(row: &Row) -> Result<SessionRecord> {
    Ok(SessionRecord {
        id: row.try_get("id")?,
        engine: Engine::parse(row.try_get("engine")?)?,
        name: row.try_get("name")?,
        goal: row.try_get("goal")?,
        cwd: row.try_get("cwd")?,
        branch: row.try_get("branch")?,
        origin: Origin::parse(row.try_get("origin")?)?,
        state: SessionState::parse(row.try_get("state")?)?,
        approvals: Approvals::parse(row.try_get("approvals")?)?,
        cli_session_id: row.try_get("cli_session_id")?,
        transcript: row.try_get("transcript")?,
        last_prompt: row.try_get("last_prompt")?,
        last_reply: row.try_get("last_reply")?,
        external_pid: row.try_get("external_pid")?,
        restore_offer: row.try_get("restore_offer")?,
        terminal_id: row.try_get("terminal_id")?,
        display_name: row.try_get("display_name")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

struct Embedded;

#[async_trait]
impl MigrationRun for Embedded {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use workforce_postgres::test_support::with_test_pool;

    pub(crate) fn record_fixture(name: &str) -> SessionRecord {
        let now = Utc::now();
        SessionRecord {
            id: Uuid::new_v4(),
            engine: Engine::Claude,
            name: name.into(),
            goal: "fix the auth race".into(),
            cwd: "/tmp/repo".into(),
            branch: None,
            origin: Origin::Desk,
            state: SessionState::Working,
            approvals: Approvals::Desk,
            cli_session_id: Some(Uuid::new_v4().to_string()),
            transcript: None,
            last_prompt: Some("run the tests".into()),
            last_reply: None,
            external_pid: None,
            restore_offer: false,
            terminal_id: None,
            display_name: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn put_get_update_and_delete_round_trip() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let mut r = record_fixture("auth-fix");
            store.put(&r).await.unwrap();
            assert_eq!(store.get(r.id).await.unwrap().as_ref(), Some(&r));

            r.state = SessionState::Interrupted;
            r.restore_offer = true;
            r.updated_at += Duration::seconds(1);
            store.put(&r).await.unwrap();
            let got = store.get(r.id).await.unwrap().unwrap();
            assert_eq!(got.state, SessionState::Interrupted);
            assert!(got.restore_offer);

            let by_cli = store
                .by_cli_session(Engine::Claude, r.cli_session_id.as_deref().unwrap())
                .await
                .unwrap();
            assert_eq!(by_cli.map(|s| s.id), Some(r.id));

            store.delete(r.id).await.unwrap();
            assert!(store.get(r.id).await.unwrap().is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn closed_sessions_are_left_out_of_the_list() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let open = record_fixture("open");
            let mut closed = record_fixture("closed");
            closed.state = SessionState::Closed;
            store.put(&open).await.unwrap();
            store.put(&closed).await.unwrap();
            let names: Vec<_> = store
                .all()
                .await
                .unwrap()
                .into_iter()
                .map(|s| s.name)
                .collect();
            assert_eq!(names, vec!["open".to_string()]);
        })
        .await;
    }
}
