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

use crate::model::{Approvals, Engine, Origin, SessionRecord, SessionState, View};
use crate::reads::ReadPlace;

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
    /// Bumped by every change to the kill switch or grants, so what reads
    /// them per call can keep what it read until they change.
    pub(crate) access_version: std::sync::atomic::AtomicU64,
}

impl DeskStore {
    /// Builds the pool and applies the schema's migrations. Fails closed: the
    /// registry has no file fall-back.
    pub async fn connect(config: &PostgresConfig) -> Result<Self> {
        let pool = build_pool(config).context("building desk pool")?;
        Self::migrate(&pool).await?;
        Ok(Self {
            pool,
            access_version: Default::default(),
        })
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
        Ok(Self {
            pool,
            access_version: Default::default(),
        })
    }

    pub(crate) fn pool(&self) -> &PostgresPool {
        &self.pool
    }

    #[cfg(test)]
    pub(crate) fn from_pool(pool: PostgresPool) -> Self {
        Self {
            pool,
            access_version: Default::default(),
        }
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
                 restore_offer, created_at, updated_at, terminal_id, display_name, asked, \
                 state_at, view, engine_version, approve_for_me, account, folder_chosen, fast, model) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20, \
                 $17, $21, $22, $23, $24, $25, $26, $27) \
                 ON CONFLICT (id) DO UPDATE SET engine = $2, name = $3, goal = $4, cwd = $5, \
                 branch = $6, origin = $7, state = $8, approvals = $9, cli_session_id = $10, \
                 transcript = $11, last_prompt = $12, last_reply = $13, external_pid = $14, \
                 restore_offer = $15, updated_at = $17, terminal_id = $18, display_name = $19, \
                 asked = $20, view = $21, engine_version = $22, approve_for_me = $23, account = $24, \
                 folder_chosen = $25, fast = $26, model = $27, \
                 state_at = CASE WHEN sessions.state = $8 THEN sessions.state_at ELSE $17 END",
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
                    &r.asked.as_ref().map(serde_json::to_value).transpose()?,
                    &r.view.as_str(),
                    &r.engine_version,
                    &r.approve_for_me,
                    &r.account,
                    &r.folder_chosen,
                    &r.fast,
                    &r.model,
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
        client
            .execute("DELETE FROM charges WHERE session = $1", &[&id])
            .await?;
        Ok(())
    }

    /// Every session found running in another terminal, closed ones too:
    /// its id and the folder it ran in.
    pub async fn external_places(&self) -> Result<Vec<(Uuid, String)>> {
        let client = self.pool.get().await?;
        let rows = client
            .query(
                "SELECT id, cwd FROM sessions WHERE origin = $1",
                &[&Origin::External.as_str()],
            )
            .await?;
        rows.iter()
            .map(|row| Ok((row.try_get("id")?, row.try_get("cwd")?)))
            .collect()
    }

    /// Forgets sessions all at once: their records, how far `owner` read
    /// each, what Activity says they or anyone did to them (but on a
    /// ticket), their lines (but what they said to other sessions), their
    /// charges, the tickets assigned and linked to them, and how far the
    /// ledger read their transcripts (what they spent is kept). How many
    /// records went.
    pub async fn forget_sessions(&self, ids: &[Uuid], owner: &str) -> Result<u64> {
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;
        let gone = forget_in(&tx, ids, owner, crate::memory_store::Memory::Forget).await?;
        tx.commit().await?;
        Ok(gone)
    }
}

/// [`DeskStore::forget_sessions`] within `tx`.
pub(crate) async fn forget_in(
    tx: &tokio_postgres::Transaction<'_>,
    ids: &[Uuid],
    owner: &str,
    memory: crate::memory_store::Memory,
) -> Result<u64> {
    // What session memory holds of them, while their records are still
    // known to tombstone; a session only cleared away keeps it.
    if memory == crate::memory_store::Memory::Forget {
        crate::memory_store::forget_memory_in(tx, ids).await?;
    }
    let places: Vec<String> = ids
        .iter()
        .map(|id| ReadPlace::Session(*id).to_string())
        .collect();
    let parties: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    // A ticket's history stays the ticket's: what a session did to it
    // outlives the session.
    tx.execute(
        "DELETE FROM activity WHERE ticket IS NULL \
         AND ((subject ->> 'kind' = 'session' AND subject ->> 'id' = ANY($1)) \
         OR (actor ->> 'kind' = 'session' AND actor ->> 'id' = ANY($1)))",
        &[&parties],
    )
    .await?;
    tx.execute(
        "UPDATE tickets SET assignee = NULL, updated_at = now(), version = version + 1 \
         WHERE assignee ->> 'kind' = 'session' AND assignee ->> 'id' = ANY($1)",
        &[&parties],
    )
    .await?;
    tx.execute(
        "UPDATE ticket_assignments SET until_at = now() \
         WHERE session = ANY($1) AND until_at IS NULL",
        &[&ids],
    )
    .await?;
    tx.execute(
        "DELETE FROM ticket_links WHERE kind = 'session' AND target = ANY($1)",
        &[&parties],
    )
    .await?;
    tx.execute(
        "DELETE FROM reads WHERE owner = $1 \
         AND EXISTS (SELECT 1 FROM unnest($2::text[]) p WHERE left(place, length(p)) = p)",
        &[&owner, &places],
    )
    .await?;
    // Its line, but what it exchanged with another session it doesn't
    // forget: that is in the other's thread too.
    tx.execute(
        "DELETE FROM line_messages WHERE \
         ((sender ->> 'kind' = 'session' AND sender ->> 'id' = ANY($1)) \
          OR (recipient ->> 'kind' = 'session' AND recipient ->> 'id' = ANY($1))) \
         AND NOT ((sender ->> 'kind' = 'session' AND NOT sender ->> 'id' = ANY($1)) \
          OR (recipient ->> 'kind' = 'session' AND NOT recipient ->> 'id' = ANY($1)))",
        &[&parties],
    )
    .await?;
    tx.execute(
        "DELETE FROM ledger_cursors WHERE session = ANY($1)",
        &[&ids],
    )
    .await?;
    tx.execute("DELETE FROM charges WHERE session = ANY($1)", &[&ids])
        .await?;
    Ok(tx
        .execute("DELETE FROM sessions WHERE id = ANY($1)", &[&ids])
        .await?)
}

/// The group session `session` (a SQL expression of a uuid) was in at
/// `at` (a SQL timestamp), for a row about it written now: where the first
/// move after `at` took it from, else the group it is in now. A row read
/// after a move about what happened before stays the old group's. Null is
/// ungrouped.
///
/// Moves are timed by the database's clock; `at` is the engine's record of
/// the call, made on this computer, which the hub runs on too. A clock that
/// disagrees by more than the gap between a call and a move puts a call
/// made just before the move in the group it moved to, or one made just
/// after in the group it left: within that skew only, never more.
pub(crate) fn group_at_sql(session: &str, at: &str) -> String {
    format!(
        "(CASE WHEN EXISTS (SELECT 1 FROM group_moves gm \
           WHERE gm.item = 'session:' || ({session})::text AND gm.at > {at}) \
         THEN (SELECT gm.from_group FROM group_moves gm \
           WHERE gm.item = 'session:' || ({session})::text AND gm.at > {at} \
           ORDER BY gm.at LIMIT 1) \
         ELSE (SELECT NULLIF(g.group_id, 'ungrouped') FROM group_members g \
           WHERE g.item = 'session:' || ({session})::text) END)"
    )
}

pub(crate) fn record(row: &Row) -> Result<SessionRecord> {
    Ok(SessionRecord {
        id: row.try_get("id")?,
        engine: Engine::parse(row.try_get("engine")?)?,
        view: View::parse(row.try_get("view")?)?,
        engine_version: row.try_get("engine_version")?,
        approve_for_me: row.try_get("approve_for_me")?,
        fast: row.try_get("fast")?,
        model: row.try_get("model")?,
        account: row.try_get("account")?,
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
        folder_chosen: row.try_get("folder_chosen")?,
        // What a newer hub kept that this one can't read is left unread.
        asked: row
            .try_get::<_, Option<serde_json::Value>>("asked")?
            .and_then(|v| serde_json::from_value(v).ok()),
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
    use chrono::{Duration, SubsecRound, Utc};
    use workforce_postgres::test_support::with_test_pool;

    pub(crate) fn record_fixture(name: &str) -> SessionRecord {
        // To the microsecond: Postgres keeps no finer, so a nanosecond
        // stamp reads back unequal to itself.
        let now = Utc::now().trunc_subsecs(6);
        SessionRecord {
            id: Uuid::new_v4(),
            engine: Engine::Claude,
            view: View::Terminal,
            engine_version: Some("2.1.300".into()),
            approve_for_me: false,
            fast: false,
            model: None,
            account: None,
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
            folder_chosen: false,
            asked: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// The registry's migrations up to `V020__access`, as a hub that ran
    /// them before Approve for me left it.
    struct UpToAccess;

    #[async_trait]
    impl MigrationRun for UpToAccess {
        async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
            embedded::migrations::runner()
                .set_target(refinery::Target::Version(20))
                .run_async(client)
                .await
                .map_err(PostgresError::from)?;
            Ok(())
        }
    }

    /// Whether the registry's sessions have the Approve for me column.
    async fn has(client: &tokio_postgres::Client) -> i64 {
        client
            .query_one(
                "SELECT count(*) FROM information_schema.columns \
                 WHERE table_schema = 'desk' AND table_name = 'sessions' \
                 AND column_name = 'approve_for_me'",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0)
    }

    #[tokio::test]
    async fn a_registry_that_ran_access_gains_approve_for_me() {
        with_test_pool(build_pool, |pool| async move {
            let set = MigrationSet::new(SCHEMA, Box::new(UpToAccess) as Box<dyn MigrationRun>);
            MigrationRunner::run(&pool, &[set]).await.unwrap();
            let client = pool.get().await.unwrap();
            assert_eq!(has(&client).await, 0, "V020__access ran, not V021");
            drop(client);
            DeskStore::migrate(&pool).await.unwrap();
            assert_eq!(has(&pool.get().await.unwrap()).await, 1);
            let store = DeskStore::from_pool(pool);
            let r = SessionRecord {
                engine: Engine::Codex,
                approve_for_me: true,
                ..record_fixture("codex")
            };
            store.put(&r).await.unwrap();
            assert!(store.get(r.id).await.unwrap().unwrap().approve_for_me);
        })
        .await;
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

            // Why it asked is kept, the refused call with it.
            r.state = SessionState::Asking;
            r.asked = crate::asking::Asked::refused(&[crate::asking::Denied::from_hook(
                "Bash",
                Some(&serde_json::json!({ "command": "git push --force" })),
                "[Git Destructive]",
            )]);
            store.put(&r).await.unwrap();
            assert_eq!(store.get(r.id).await.unwrap().as_ref(), Some(&r));

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
    async fn sessions_are_forgotten_at_once_with_how_far_they_were_read() {
        with_test_pool(build_pool, |pool| async move {
            DeskStore::migrate(&pool).await.unwrap();
            let store = DeskStore::from_pool(pool);
            let mut gone = record_fixture("gone");
            gone.origin = Origin::External;
            gone.state = SessionState::Closed;
            let mut open = record_fixture("open");
            open.origin = Origin::External;
            let kept = record_fixture("kept");
            for r in [&gone, &open, &kept] {
                store.put(r).await.unwrap();
            }
            let mut places = store.external_places().await.unwrap();
            places.sort();
            let mut expected = vec![(gone.id, gone.cwd.clone()), (open.id, open.cwd.clone())];
            expected.sort();
            assert_eq!(
                places, expected,
                "closed ones too, and only another terminal's"
            );

            let read = |id| crate::reads::ReadPut {
                place: ReadPlace::Session(id),
                point: crate::reads::ReadPoint {
                    at: Utc::now(),
                    message: None,
                },
                mark: crate::reads::Mark::Advance,
            };
            store
                .put_reads("you", &[read(gone.id), read(kept.id)])
                .await
                .unwrap();
            let party = |r: &SessionRecord| crate::crew::Party {
                kind: crate::crew::PartyKind::Session,
                id: r.id.to_string(),
                name: r.name.clone(),
                engine: Some(r.engine),
                origin: Some(r.origin),
            };
            for r in [&gone, &kept] {
                let started = crate::crew::Event::new(
                    crate::crew::Kind::Started,
                    party(r),
                    Some(party(r)),
                    "started",
                );
                store.add_event(&started).await.unwrap();
            }
            let stopped = crate::crew::Event::new(
                crate::crew::Kind::Stopped,
                crate::crew::Party::you(),
                Some(party(&open)),
                "stopped it",
            );
            store.add_event(&stopped).await.unwrap();

            assert_eq!(
                store
                    .forget_sessions(&[gone.id, open.id], "you")
                    .await
                    .unwrap(),
                2
            );
            assert!(store.get(gone.id).await.unwrap().is_none());
            assert!(store.get(open.id).await.unwrap().is_none());
            assert!(store.get(kept.id).await.unwrap().is_some());
            let places: Vec<ReadPlace> = store
                .reads("you")
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.place)
                .collect();
            assert_eq!(places, [ReadPlace::Session(kept.id)]);
            let (events, _) = store
                .events(crate::crew::Filter::All, None, 50)
                .await
                .unwrap();
            let about: Vec<String> = events
                .into_iter()
                .filter_map(|e| e.subject.map(|s| s.name))
                .collect();
            assert_eq!(about, ["kept"], "what Activity says of them goes with them");
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
