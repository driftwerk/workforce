//! The floor's tables in Postgres, schema `floor`.

use std::collections::HashSet;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::GenericClient;
use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;
use workforce_common::session_postgres::{PostgresSessionStore, HEARTBEAT_STALE_SECS};
use workforce_common::usage::{LoginRef, PlanKind, Reading};
use workforce_desk::floor::{
    self, Gists, Outcome, Scope, Settling, SubjectKey, Target, Turn, TurnOp,
};
use workforce_desk::gist::{Gist, Paragraph};
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};

// Own directory, never a subdir of another embed root: refinery's
// embed_migrations is recursive.
mod embedded {
    refinery::embed_migrations!("migrations_floor");
}

struct Migrations;

#[async_trait]
impl MigrationRun for Migrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

const SEARCH_PATH: &str = "SET search_path TO floor, public";

/// Rows one prune statement removes.
const PRUNE_BATCH: i64 = 5_000;

const COLUMNS: &str = "id, owner, subject_doc, name, place, task, doing, started_at, heard_at, \
                       ended_at, outcome, answer, question, settled_at, waits, token, instance, idle";

const GIST_COLUMNS: &str = "purpose, purpose_at, now_text, now_at, turn, by_model";

/// How many of a subject's first and latest asks its purpose is read from.
const FIRST_ASKS: i64 = 2;
const LATEST_ASKS: i64 = 4;

pub struct PgFloorStore {
    pool: PostgresPool,
}

impl PgFloorStore {
    pub const SCHEMA: &'static str = "floor";

    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        PostgresPool::from_config_with_session_init(config, &[SEARCH_PATH])
    }

    pub async fn new(config: &PostgresConfig) -> Result<Self> {
        let pool = Self::build_pool(config).context("building the floor's pool")?;
        Self::with_pool(pool).await
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
        .context("running the floor's migrations")?;
        Ok(Self { pool })
    }

    /// Writes `op` with the log's semantics ([`floor::apply`]): settling in
    /// SQL across the subject, the turn itself read, changed and written back
    /// under its row lock. Whose turns it wrote, if it wrote any: an op only
    /// ever touches one owner's (`None`, a personal hub's Desk sessions).
    pub async fn apply(&self, op: &TurnOp) -> Result<Option<Option<String>>> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        if let Some(settling) = floor::settles(op) {
            settle(&tx, &settling).await?;
        }
        let whose = match (op, op.turn()) {
            (TurnOp::Begin { turn, .. }, _) => {
                insert(&tx, turn).await?;
                Some(turn.owner.clone())
            }
            (TurnOp::Settle { owner, .. }, _) => Some(owner.clone()),
            (_, Some(id)) => {
                let row = tx
                    .query_opt(
                        &format!(
                            "SELECT {COLUMNS} FROM turns WHERE id = $1 AND ended_at IS NULL FOR UPDATE"
                        ),
                        &[&id],
                    )
                    .await?;
                let changed = row
                    .as_ref()
                    .map(turn_of)
                    .transpose()?
                    .and_then(|turn| floor::apply(vec![turn], op).pop());
                if let Some(changed) = &changed {
                    update(&tx, changed).await?;
                }
                changed.map(|t| t.owner)
            }
            (_, None) => None,
        };
        tx.commit().await?;
        Ok(whose)
    }

    /// Turns of `scope` started or ended since `since`, every open one, and
    /// every question still unanswered, however old.
    pub async fn relevant(&self, scope: &Scope, since: DateTime<Utc>) -> Result<Vec<Turn>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM turns \
                     WHERE (owner = $1 OR ($2 AND owner IS NULL)) \
                       AND (started_at >= $3 OR ended_at IS NULL OR ended_at >= $3 \
                            OR (outcome = 'asked' AND settled_at IS NULL)) \
                     ORDER BY started_at"
                ),
                &[&scope.owner, &scope.personal, &since],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                turn_of(row)
                    .inspect_err(|e| {
                        tracing::warn!(target: "floor", error = %format!("{e:#}"), "skipping a turn this hub can't read")
                    })
                    .ok()
            })
            .collect())
    }

    /// One turn, as logged.
    pub async fn turn(&self, id: Uuid) -> Result<Option<Turn>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                &format!("SELECT {COLUMNS} FROM turns WHERE id = $1"),
                &[&id],
            )
            .await?;
        row.as_ref().map(turn_of).transpose()
    }

    /// What `owner`'s `subject` was asked: its first asks and its latest,
    /// oldest first.
    pub async fn asks(&self, owner: Option<&str>, subject: &SubjectKey) -> Result<Vec<String>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT task FROM ( \
                     (SELECT task, started_at FROM turns \
                      WHERE owner IS NOT DISTINCT FROM $1 AND subject = $2 AND task IS NOT NULL \
                      ORDER BY started_at LIMIT $3) \
                     UNION \
                     (SELECT task, started_at FROM turns \
                      WHERE owner IS NOT DISTINCT FROM $1 AND subject = $2 AND task IS NOT NULL \
                      ORDER BY started_at DESC LIMIT $4)) asked \
                 ORDER BY started_at",
                &[&owner, &subject.as_str(), &FIRST_ASKS, &LATEST_ASKS],
            )
            .await?;
        let mut asks: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
        asks.dedup();
        Ok(asks)
    }

    /// The gists of `scope`'s subjects.
    pub async fn gists(&self, scope: &Scope) -> Result<Gists> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                &format!(
                    "SELECT subject, {GIST_COLUMNS} FROM gists \
                     WHERE owner = $1 OR ($2 AND owner IS NULL)"
                ),
                &[&scope.owner, &scope.personal],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|row| {
                (
                    SubjectKey::kept(row.get::<_, String>("subject")),
                    gist_of(row),
                )
            })
            .collect())
    }

    /// `owner`'s gist of `subject`.
    pub async fn gist(&self, owner: Option<&str>, subject: &SubjectKey) -> Result<Option<Gist>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                &format!(
                    "SELECT {GIST_COLUMNS} FROM gists \
                     WHERE owner IS NOT DISTINCT FROM $1 AND subject = $2"
                ),
                &[&owner, &subject.as_str()],
            )
            .await?;
        Ok(row.as_ref().map(gist_of))
    }

    /// Keeps `gist` as `owner`'s of `subject`, unless the one kept was
    /// written later: replicas each write what they saw.
    pub async fn put_gist(
        &self,
        owner: Option<&str>,
        subject: &SubjectKey,
        gist: &Gist,
    ) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            &format!(
                "INSERT INTO gists (owner, subject, {GIST_COLUMNS}) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT ((coalesce(owner, '')), subject) DO UPDATE SET \
                     purpose = EXCLUDED.purpose, purpose_at = EXCLUDED.purpose_at, \
                     now_text = EXCLUDED.now_text, now_at = EXCLUDED.now_at, \
                     turn = EXCLUDED.turn, by_model = EXCLUDED.by_model \
                 WHERE gists.now_at <= EXCLUDED.now_at"
            ),
            &[
                &owner,
                &subject.as_str(),
                &gist.purpose,
                &gist.purpose_at,
                &gist.now,
                &gist.now_at,
                &gist.turn,
                &gist.by_model,
            ],
        )
        .await?;
        Ok(())
    }

    /// The floor's paragraph for `owner` (`personal`: with their hub's own
    /// sessions), and the mark of what it was written from.
    pub async fn summary(&self, owner: &str, personal: bool) -> Result<Option<(Paragraph, i64)>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT paragraph, written_at, fingerprint FROM summaries \
                 WHERE owner = $1 AND personal = $2",
                &[&owner, &personal],
            )
            .await?;
        Ok(row.map(|row| {
            (
                Paragraph {
                    text: row.get(0),
                    at: row.get(1),
                },
                row.get(2),
            )
        }))
    }

    /// Keeps `paragraph` as `owner`'s, unless the one kept was written later.
    pub async fn put_summary(
        &self,
        owner: &str,
        personal: bool,
        paragraph: &Paragraph,
        fingerprint: i64,
    ) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "INSERT INTO summaries (owner, personal, paragraph, written_at, fingerprint) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (owner, personal) DO UPDATE SET paragraph = EXCLUDED.paragraph, \
                 written_at = EXCLUDED.written_at, fingerprint = EXCLUDED.fingerprint \
             WHERE summaries.written_at <= EXCLUDED.written_at",
            &[
                &owner,
                &personal,
                &paragraph.text,
                &paragraph.at,
                &fingerprint,
            ],
        )
        .await?;
        Ok(())
    }

    /// Which of `instances` still run: their heartbeat in the sessions'
    /// roster is fresh. An instance's id is new on every boot, so one whose
    /// beat went stale, was pruned, or never came has gone.
    pub async fn alive(&self, instances: &[String]) -> Result<HashSet<String>> {
        if instances.is_empty() {
            return Ok(HashSet::new());
        }
        let conn = self.pool.get().await?;
        let asked = conn
            .query(
                &format!(
                    "SELECT instance_id FROM {}.instance_heartbeats \
                     WHERE instance_id = ANY($1) \
                       AND last_seen >= now() - make_interval(secs => $2::double precision)",
                    PostgresSessionStore::SCHEMA
                ),
                &[&instances, &(HEARTBEAT_STALE_SECS as f64)],
            )
            .await;
        match asked {
            Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) => {
                Ok(HashSet::new())
            }
            asked => Ok(asked?.iter().map(|row| row.get(0)).collect()),
        }
    }

    /// Keeps `r` as its login's latest reading; whether it was later than
    /// the one kept.
    pub async fn record(&self, r: &Reading) -> Result<bool> {
        let owner = r.login.owner().map(str::to_string);
        let conn = self.pool.get().await?;
        // Replicas write as they see readings: only a later one replaces
        // what is kept, so the newest any of them saw is what all serve.
        let kept = conn.execute(
            "INSERT INTO plan_readings (login, kind, owner, account, plan, windows, observed_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (login) DO UPDATE SET kind = EXCLUDED.kind, owner = EXCLUDED.owner, \
                 account = COALESCE(EXCLUDED.account, plan_readings.account), \
                 plan = COALESCE(EXCLUDED.plan, plan_readings.plan), \
                 windows = EXCLUDED.windows, observed_at = EXCLUDED.observed_at \
             WHERE plan_readings.observed_at < EXCLUDED.observed_at",
            &[
                &login_key(r),
                &r.kind.as_str(),
                &owner,
                &r.account,
                &r.plan,
                &serde_json::to_value(&r.windows)?,
                &r.observed_at,
            ],
        )
        .await?;
        Ok(kept == 1)
    }

    /// Every login's latest reading.
    pub async fn readings(&self) -> Result<Vec<Reading>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT login, kind, owner, account, plan, windows, observed_at \
                 FROM plan_readings ORDER BY observed_at DESC",
                &[],
            )
            .await?;
        Ok(rows.iter().filter_map(reading_of).collect())
    }

    /// For everyone the floor keeps turns or plan readings of, how many.
    pub async fn holders(&self) -> Result<std::collections::BTreeMap<String, usize>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT owner, count(*) FROM (SELECT owner FROM turns \
                 UNION ALL SELECT owner FROM plan_readings \
                 UNION ALL SELECT owner FROM gists UNION ALL SELECT owner FROM summaries) kept \
                 WHERE owner IS NOT NULL GROUP BY owner",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|row| {
                let held: i64 = row.get(1);
                (row.get(0), usize::try_from(held).unwrap_or_default())
            })
            .collect())
    }

    /// How many turns and plan readings `person` has.
    pub async fn held(&self, person: &str) -> Result<(u64, u64)> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_one(
                "SELECT (SELECT count(*) FROM turns WHERE owner = $1), \
                        (SELECT count(*) FROM plan_readings WHERE owner = $1)",
                &[&person],
            )
            .await?;
        let count = |i| u64::try_from(row.get::<_, i64>(i)).unwrap_or_default();
        Ok((count(0), count(1)))
    }

    /// Makes `from`'s turns, plan readings and gists `into`'s; how many
    /// moved. Where both have a gist of one subject, `into`'s stays; their
    /// paragraphs are written again.
    pub async fn reassign(&self, from: &str, into: &str) -> Result<u64> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        tx.execute(
            "DELETE FROM gists g WHERE owner = $1 \
             AND EXISTS (SELECT 1 FROM gists h WHERE h.owner = $2 AND h.subject = g.subject)",
            &[&from, &into],
        )
        .await?;
        tx.execute("DELETE FROM summaries WHERE owner = $1", &[&from])
            .await?;
        let mut moved = 0;
        for table in ["turns", "plan_readings", "gists"] {
            moved += tx
                .execute(
                    &format!("UPDATE {table} SET owner = $2 WHERE owner = $1"),
                    &[&from, &into],
                )
                .await?;
        }
        tx.commit().await?;
        Ok(moved)
    }

    /// Removes `person`'s turns, plan readings, gists and paragraphs; how
    /// many went.
    pub async fn forget(&self, person: &str) -> Result<u64> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let mut removed = 0;
        for table in ["turns", "plan_readings", "gists", "summaries"] {
            removed += tx
                .execute(&format!("DELETE FROM {table} WHERE owner = $1"), &[&person])
                .await?;
        }
        tx.commit().await?;
        Ok(removed)
    }

    /// Removes turns started before `before`, except questions still
    /// waiting on the person, and readings, gists and paragraphs older than
    /// it; returns how many went.
    pub async fn prune(&self, before: DateTime<Utc>) -> Result<u64> {
        let conn = self.pool.get().await?;
        let mut removed = 0;
        for older in [
            "DELETE FROM plan_readings WHERE observed_at < $1",
            "DELETE FROM gists WHERE now_at < $1",
            "DELETE FROM summaries WHERE written_at < $1",
        ] {
            removed += conn.execute(older, &[&before]).await?;
        }
        loop {
            let batch = conn
                .execute(
                    "DELETE FROM turns WHERE id IN ( \
                         SELECT id FROM turns WHERE started_at < $1 \
                           AND (outcome IS DISTINCT FROM 'asked' OR settled_at IS NOT NULL) \
                         LIMIT $2)",
                    &[&before, &PRUNE_BATCH],
                )
                .await?;
            removed += batch;
            if batch < PRUNE_BATCH as u64 {
                return Ok(removed);
            }
        }
    }
}

/// The key a reading is kept under: `hub:<login>`, or `mac:<kind>`.
pub(super) fn login_key(r: &Reading) -> String {
    match &r.login {
        LoginRef::Hub { login, .. } => format!("hub:{login}"),
        LoginRef::Mac => format!("mac:{}", r.kind.as_str()),
    }
}

/// A kept reading; `None` for one this hub can't read.
fn reading_of(row: &Row) -> Option<Reading> {
    let key: String = row.get("login");
    let owner: Option<String> = row.get("owner");
    let login = match key.split_once(':') {
        Some(("mac", _)) => LoginRef::Mac,
        Some(("hub", login)) => LoginRef::Hub {
            login: login.to_string(),
            owner,
        },
        _ => return None,
    };
    Some(Reading {
        kind: PlanKind::parse(row.get("kind"))?,
        login,
        account: row.get("account"),
        plan: row.get("plan"),
        windows: serde_json::from_value(row.get("windows")).ok()?,
        observed_at: row.get("observed_at"),
    })
}

/// Holds, to the end of the transaction, the questions of the owner and
/// subject `s` settles, so two replicas settling one subject at once each
/// see the other's question.
async fn lock_subject(db: &impl GenericClient, s: &Settling) -> Result<()> {
    let key: Option<(Option<String>, String)> = match &s.target {
        Target::Subject { subject, owner } => Some((owner.clone(), subject.as_str().to_string())),
        Target::EarlierThan(id) => db
            .query_opt("SELECT owner, subject FROM turns WHERE id = $1", &[id])
            .await?
            .map(|row| (row.get(0), row.get(1))),
    };
    if let Some((owner, subject)) = key {
        db.execute(
            "SELECT pg_advisory_xact_lock(hashtextextended(coalesce($1, '') || '/' || $2, 0))",
            &[&owner, &subject],
        )
        .await?;
    }
    Ok(())
}

async fn settle(db: &impl GenericClient, s: &Settling) -> Result<()> {
    lock_subject(db, s).await?;
    match &s.target {
        Target::Subject { subject, owner } => {
            db.execute(
                "UPDATE turns SET settled_at = $1 \
                 WHERE owner IS NOT DISTINCT FROM $2 AND subject = $3 \
                   AND outcome = 'asked' AND settled_at IS NULL",
                &[&s.at, owner, &subject.as_str()],
            )
            .await?
        }
        Target::EarlierThan(id) => {
            db.execute(
                "UPDATE turns t SET settled_at = $1 FROM turns me \
                 WHERE me.id = $2 AND t.id <> me.id \
                   AND t.owner IS NOT DISTINCT FROM me.owner AND t.subject = me.subject \
                   AND t.outcome = 'asked' AND t.settled_at IS NULL",
                &[&s.at, id],
            )
            .await?
        }
    };
    Ok(())
}

async fn insert(db: &impl GenericClient, t: &Turn) -> Result<()> {
    db.execute(
        &format!(
            "INSERT INTO turns ({COLUMNS}, subject) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19) \
             ON CONFLICT (id) DO NOTHING"
        ),
        &[
            &t.id,
            &t.owner,
            &serde_json::to_value(&t.subject)?,
            &t.name,
            &serde_json::to_value(&t.place)?,
            &t.task,
            &t.doing,
            &t.started_at,
            &t.heard_at,
            &t.ended_at,
            &t.outcome.map(Outcome::as_str),
            &t.answer,
            &t.question.as_ref().map(serde_json::to_value).transpose()?,
            &t.settled_at,
            &serde_json::to_value(&t.waits)?,
            &t.token,
            &t.instance,
            &serde_json::to_value(&t.idle)?,
            &t.subject.key().as_str(),
        ],
    )
    .await?;
    Ok(())
}

/// Writes back what a turn's own ops change.
async fn update(db: &impl GenericClient, t: &Turn) -> Result<()> {
    db.execute(
        "UPDATE turns SET doing = $2, heard_at = $3, ended_at = $4, outcome = $5, \
             answer = $6, question = $7, waits = $8, idle = $9 \
         WHERE id = $1",
        &[
            &t.id,
            &t.doing,
            &t.heard_at,
            &t.ended_at,
            &t.outcome.map(Outcome::as_str),
            &t.answer,
            &t.question.as_ref().map(serde_json::to_value).transpose()?,
            &serde_json::to_value(&t.waits)?,
            &serde_json::to_value(&t.idle)?,
        ],
    )
    .await?;
    Ok(())
}

fn gist_of(row: &Row) -> Gist {
    Gist {
        purpose: row.get("purpose"),
        purpose_at: row.get("purpose_at"),
        now: row.get("now_text"),
        now_at: row.get("now_at"),
        turn: row.get("turn"),
        by_model: row.get("by_model"),
    }
}

fn turn_of(row: &Row) -> Result<Turn> {
    Ok(Turn {
        id: row.get::<_, Uuid>("id"),
        owner: row.get("owner"),
        subject: serde_json::from_value(row.get("subject_doc"))?,
        name: row.get("name"),
        place: serde_json::from_value(row.get("place"))?,
        task: row.get("task"),
        doing: row.get("doing"),
        started_at: row.get("started_at"),
        heard_at: row.get("heard_at"),
        ended_at: row.get("ended_at"),
        outcome: row
            .get::<_, Option<String>>("outcome")
            .map(|o| Outcome::parse(&o))
            .transpose()?,
        answer: row.get("answer"),
        question: row
            .get::<_, Option<serde_json::Value>>("question")
            .map(serde_json::from_value)
            .transpose()?,
        settled_at: row.get("settled_at"),
        waits: serde_json::from_value(row.get("waits"))?,
        token: row.get("token"),
        instance: row.get("instance"),
        idle: serde_json::from_value(row.get("idle"))?,
    })
}

#[cfg(test)]
mod tests {
    use workforce_desk::crew::ItemQuestion;
    use workforce_desk::floor::{Answer, Place, Subject, SubjectKey, Wait};
    use workforce_postgres::test_support::with_test_pool;

    use super::*;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339).unwrap().to_utc()
    }

    fn turn(agent: &str, owner: Option<&str>, from: &str) -> Turn {
        Turn {
            id: Uuid::new_v4(),
            owner: owner.map(str::to_string),
            subject: Subject::Agent {
                agent_id: agent.into(),
                session_id: "s1".into(),
            },
            name: "Scout".into(),
            place: Place::Dm,
            task: Some("Compare spend.".into()),
            doing: None,
            started_at: at(from),
            heard_at: at(from),
            ended_at: None,
            outcome: None,
            answer: None,
            question: None,
            settled_at: None,
            waits: vec![],
            idle: vec![],
            token: Some("tok".into()),
            instance: Some("hub-1".into()),
        }
    }

    fn begin(t: &Turn, by_person: bool) -> TurnOp {
        TurnOp::Begin {
            turn: Box::new(t.clone()),
            by_person,
        }
    }

    fn asks(t: &Turn, when: &str) -> TurnOp {
        TurnOp::End {
            turn: t.id,
            at: at(when),
            outcome: Outcome::Asked,
            answer: Some("Ready.".into()),
            question: Some(ItemQuestion {
                text: "Ship it?".into(),
                options: Some(vec!["yes".into(), "no".into()]),
            }),
        }
    }

    fn scope(owner: &str) -> Scope {
        Scope {
            owner: owner.into(),
            personal: false,
        }
    }

    async fn store(pool: PostgresPool) -> PgFloorStore {
        PgFloorStore::with_pool(pool).await.expect("store")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_persons_turns_and_readings_move_with_them_and_go_when_they_do() {
        use workforce_common::usage::{LoginRef, PlanKind, Reading};
        with_test_pool(PgFloorStore::build_pool, |pool| async move {
            let store = store(pool).await;
            for (owner, from) in [
                (Some("u1"), "2026-10-29T08:00:00Z"),
                (Some("u1"), "2026-10-29T09:00:00Z"),
                (Some("u2"), "2026-10-29T09:30:00Z"),
                (None, "2026-10-29T10:00:00Z"),
            ] {
                store
                    .apply(&begin(&turn("scout", owner, from), false))
                    .await
                    .unwrap();
            }
            store
                .record(&Reading {
                    kind: PlanKind::Claude,
                    login: LoginRef::Hub {
                        login: "k1".into(),
                        owner: Some("u1".into()),
                    },
                    account: None,
                    plan: None,
                    windows: vec![],
                    observed_at: at("2026-10-29T10:00:00Z"),
                })
                .await
                .unwrap();
            assert_eq!(
                store.holders().await.unwrap(),
                std::collections::BTreeMap::from([("u1".to_string(), 3), ("u2".to_string(), 1)]),
                "a personal hub's own sessions are no one's"
            );
            assert_eq!(store.held("u1").await.unwrap(), (2, 1));
            assert_eq!(store.reassign("u1", "u3").await.unwrap(), 3);
            assert_eq!(store.held("u1").await.unwrap(), (0, 0));
            assert_eq!(store.held("u3").await.unwrap(), (2, 1));
            assert_eq!(store.forget("u3").await.unwrap(), 3);
            assert_eq!(store.held("u3").await.unwrap(), (0, 0));
            assert_eq!(store.held("u2").await.unwrap(), (1, 0));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_store_keeps_the_logs_semantics() {
        with_test_pool(PgFloorStore::build_pool, |pool| async move {
            let store = store(pool).await;
            let first = turn("scout", Some("u1"), "2026-10-29T08:00:00Z");
            let second = turn("scout", Some("u1"), "2026-10-29T09:00:00Z");
            let theirs = turn("scout", Some("u2"), "2026-10-29T08:30:00Z");
            let wait = Wait {
                key: "r1".into(),
                from: at("2026-10-29T09:05:00Z"),
                to: None,
                ask: "Run cargo test?".into(),
                answer: Answer::Reply,
            };
            let ops = vec![
                begin(&first, true),
                asks(&first, "2026-10-29T08:10:00Z"),
                begin(&theirs, true),
                asks(&theirs, "2026-10-29T08:40:00Z"),
                begin(&second, false),
                TurnOp::Progress {
                    turn: second.id,
                    doing: Some("● Bash cargo test".into()),
                    at: at("2026-10-29T09:01:00Z"),
                },
                TurnOp::Wait {
                    turn: second.id,
                    wait: wait.clone(),
                },
                TurnOp::Wait {
                    turn: second.id,
                    wait: Wait {
                        answer: Answer::Terminal,
                        ..wait.clone()
                    },
                },
                TurnOp::Unwait {
                    turn: second.id,
                    key: "r1".into(),
                    at: at("2026-10-29T09:06:00Z"),
                },
                TurnOp::Wait {
                    turn: second.id,
                    wait: Wait {
                        key: "r2".into(),
                        ..wait
                    },
                },
                TurnOp::Background {
                    turn: second.id,
                    count: 2,
                    at: at("2026-10-29T09:07:00Z"),
                },
                asks(&second, "2026-10-29T09:10:00Z"),
            ];
            for op in &ops {
                let whose = store.apply(op).await.unwrap();
                assert!(whose.is_some(), "every op here writes a turn: {op:?}");
            }
            let ended = TurnOp::Unwait {
                turn: first.id,
                key: "r1".into(),
                at: at("2026-10-29T10:00:00Z"),
            };
            assert_eq!(
                store.apply(&ended).await.unwrap(),
                None,
                "an ended turn is not written to, so nobody's floor changed"
            );
            let expected = ops.iter().fold(Vec::new(), floor::apply);
            let since = at("2026-10-29T00:00:00Z");
            let mine = store.relevant(&scope("u1"), since).await.unwrap();
            let want: Vec<Turn> = expected
                .iter()
                .filter(|t| t.owner.as_deref() == Some("u1"))
                .cloned()
                .collect();
            assert_eq!(mine, want);
            assert!(!mine[0].asking(), "the newer question settled the earlier");
            assert!(mine[1].asking());
            assert_eq!(mine[1].waits[1].to, Some(at("2026-10-29T09:10:00Z")));
            assert_eq!(mine[1].idle[0].to, Some(at("2026-10-29T09:10:00Z")));
            let theirs_now = store.relevant(&scope("u2"), since).await.unwrap();
            assert!(theirs_now[0].asking(), "only u1's questions settled");

            let long = turn("scout", Some("u3"), "2026-10-28T20:00:00Z");
            store.apply(&begin(&long, false)).await.unwrap();
            store
                .apply(&TurnOp::End {
                    turn: long.id,
                    at: at("2026-10-29T01:00:00Z"),
                    outcome: Outcome::Finished,
                    answer: Some("Done.".into()),
                    question: None,
                })
                .await
                .unwrap();
            assert_eq!(
                store.relevant(&scope("u3"), since).await.unwrap()[0].id,
                long.id,
                "a turn begun before and ended within the window is read"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_person_speaking_settles_and_a_job_does_not() {
        with_test_pool(PgFloorStore::build_pool, |pool| async move {
            let store = store(pool).await;
            let asked = turn("scout", Some("u1"), "2026-10-29T08:00:00Z");
            store.apply(&begin(&asked, true)).await.unwrap();
            store
                .apply(&asks(&asked, "2026-10-29T08:10:00Z"))
                .await
                .unwrap();
            let since = at("2026-10-29T00:00:00Z");
            let asking = |turns: Vec<Turn>| turns.iter().filter(|t| t.asking()).count();
            let job = turn("scout", Some("u1"), "2026-10-29T09:00:00Z");
            store.apply(&begin(&job, false)).await.unwrap();
            assert_eq!(
                asking(store.relevant(&scope("u1"), since).await.unwrap()),
                1
            );
            let reply = turn("scout", Some("u1"), "2026-10-29T10:00:00Z");
            store.apply(&begin(&reply, true)).await.unwrap();
            assert_eq!(
                asking(store.relevant(&scope("u1"), since).await.unwrap()),
                0
            );

            let session = Turn {
                owner: None,
                subject: Subject::Session {
                    id: Uuid::new_v4(),
                    engine: workforce_desk::model::Engine::Claude,
                },
                ..turn("x", None, "2026-10-29T11:00:00Z")
            };
            store.apply(&begin(&session, true)).await.unwrap();
            store
                .apply(&asks(&session, "2026-10-29T11:10:00Z"))
                .await
                .unwrap();
            let personal = Scope {
                personal: true,
                ..scope("u1")
            };
            assert_eq!(asking(store.relevant(&personal, since).await.unwrap()), 1);
            assert_eq!(
                asking(store.relevant(&scope("u1"), since).await.unwrap()),
                0,
                "a Desk session is only the personal hub's"
            );
            store
                .apply(&TurnOp::Settle {
                    subject: session.subject.key(),
                    owner: None,
                    at: at("2026-10-29T11:20:00Z"),
                })
                .await
                .unwrap();
            assert_eq!(asking(store.relevant(&personal, since).await.unwrap()), 0);
            assert_ne!(session.subject.key(), SubjectKey::agent("x"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn old_questions_stay_until_answered_and_the_rest_are_pruned() {
        with_test_pool(PgFloorStore::build_pool, |pool| async move {
            let store = store(pool).await;
            let unanswered = turn("scout", Some("u1"), "2026-10-01T08:00:00Z");
            let answered = turn("lens", Some("u1"), "2026-10-01T08:00:00Z");
            let finished = turn("scout", Some("u1"), "2026-10-02T08:00:00Z");
            let recent = turn("scout", Some("u1"), "2026-10-29T08:00:00Z");
            for op in [
                begin(&unanswered, false),
                asks(&unanswered, "2026-10-01T08:10:00Z"),
                begin(&answered, false),
                asks(&answered, "2026-10-01T08:10:00Z"),
                TurnOp::Settle {
                    subject: answered.subject.key(),
                    owner: Some("u1".into()),
                    at: at("2026-10-01T09:00:00Z"),
                },
                begin(&finished, false),
                TurnOp::End {
                    turn: finished.id,
                    at: at("2026-10-02T08:10:00Z"),
                    outcome: Outcome::Finished,
                    answer: None,
                    question: None,
                },
                begin(&recent, false),
            ] {
                store.apply(&op).await.unwrap();
            }
            let ids = |turns: Vec<Turn>| turns.iter().map(|t| t.id).collect::<Vec<_>>();
            let since = at("2026-10-28T00:00:00Z");
            assert_eq!(
                ids(store.relevant(&scope("u1"), since).await.unwrap()),
                [unanswered.id, recent.id]
            );
            assert_eq!(store.prune(at("2026-10-21T00:00:00Z")).await.unwrap(), 2);
            assert_eq!(
                ids(store
                    .relevant(&scope("u1"), at("2026-01-01T00:00:00Z"))
                    .await
                    .unwrap()),
                [unanswered.id, recent.id]
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_instance_runs_while_its_heartbeat_in_the_sessions_roster_is_fresh() {
        with_test_pool(PgFloorStore::build_pool, |pool| async move {
            let store = store(pool.clone()).await;
            let instances = ["hub-1", "hub-2", "hub-3"].map(String::from);
            assert!(
                store.alive(&instances).await.unwrap().is_empty(),
                "without a roster no other instance is known to run"
            );
            pool.get()
                .await
                .unwrap()
                .batch_execute(
                    "CREATE SCHEMA IF NOT EXISTS sessions; \
                     CREATE TABLE sessions.instance_heartbeats \
                         (instance_id TEXT PRIMARY KEY, last_seen TIMESTAMPTZ NOT NULL); \
                     INSERT INTO sessions.instance_heartbeats VALUES \
                         ('hub-1', now()), ('hub-2', now() - interval '1 hour')",
                )
                .await
                .unwrap();
            assert_eq!(
                store.alive(&instances).await.unwrap(),
                HashSet::from(["hub-1".to_string()]),
                "a stale beat and none at all both mean gone"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_logins_reading_only_moves_forward_and_keeps_what_it_knew() {
        use workforce_common::usage::{LoginRef, PlanKind, Reading, Window};
        with_test_pool(PgFloorStore::build_pool, |pool| async move {
            let store = store(pool).await;
            let reading = |used: f64, when: &str, plan: Option<&str>| Reading {
                kind: PlanKind::Chatgpt,
                login: LoginRef::Hub {
                    login: "k1".into(),
                    owner: Some("u1".into()),
                },
                account: plan.map(|_| "acct".to_string()),
                plan: plan.map(str::to_string),
                windows: vec![Window {
                    used_percent: used,
                    resets_at: Some(at("2026-10-29T19:10:00Z")),
                    minutes: 300,
                }],
                observed_at: at(when),
            };
            assert!(store
                .record(&reading(40.0, "2026-10-29T16:00:00Z", Some("pro")))
                .await
                .unwrap());
            assert!(
                !store
                    .record(&reading(10.0, "2026-10-29T15:00:00Z", Some("plus")))
                    .await
                    .unwrap(),
                "an older reading is not kept"
            );
            assert!(store
                .record(&reading(45.0, "2026-10-29T16:05:00Z", None))
                .await
                .unwrap());
            let mac = Reading {
                login: LoginRef::Mac,
                kind: PlanKind::Claude,
                ..reading(5.0, "2026-10-29T16:01:00Z", None)
            };
            store.record(&mac).await.unwrap();
            let kept = store.readings().await.unwrap();
            assert_eq!(kept.len(), 2);
            let hub = &kept[0];
            assert_eq!(
                hub.windows[0].used_percent, 45.0,
                "an older reading never wins"
            );
            assert_eq!(
                (hub.plan.as_deref(), hub.account.as_deref()),
                (Some("pro"), Some("acct")),
                "a reading that doesn't say keeps what was known"
            );
            assert_eq!(kept[1], mac);
            assert_eq!(store.prune(at("2026-10-29T16:02:00Z")).await.unwrap(), 1);
            assert_eq!(store.readings().await.unwrap().len(), 1);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_subjects_gist_is_kept_latest_first_and_goes_with_its_person() {
        with_test_pool(PgFloorStore::build_pool, |pool| async move {
            let store = store(pool).await;
            let scout = SubjectKey::agent("scout");
            let gist = |now: &str, when: &str| Gist {
                purpose: Some("Getting the release out.".into()),
                purpose_at: Some(at("2026-10-29T08:00:00Z")),
                now: now.into(),
                now_at: at(when),
                turn: Uuid::nil(),
                by_model: true,
            };
            store
                .put_gist(
                    Some("u1"),
                    &scout,
                    &gist("Testing it.", "2026-10-29T09:00:00Z"),
                )
                .await
                .unwrap();
            store
                .put_gist(
                    Some("u1"),
                    &scout,
                    &gist("Earlier.", "2026-10-29T08:30:00Z"),
                )
                .await
                .unwrap();
            store
                .put_gist(
                    None,
                    &SubjectKey::session(Uuid::nil()),
                    &gist("Mine.", "2026-10-29T09:00:00Z"),
                )
                .await
                .unwrap();
            store
                .put_gist(Some("u2"), &scout, &gist("Theirs.", "2026-10-29T09:00:00Z"))
                .await
                .unwrap();
            assert_eq!(
                store.gist(Some("u1"), &scout).await.unwrap(),
                Some(gist("Testing it.", "2026-10-29T09:00:00Z")),
                "an earlier gist never replaces a later one"
            );
            let personal = Scope {
                personal: true,
                ..scope("u1")
            };
            let mine = store.gists(&personal).await.unwrap();
            assert_eq!(mine.len(), 2);
            assert_eq!(mine[&SubjectKey::session(Uuid::nil())].now, "Mine.");
            assert_eq!(store.gists(&scope("u1")).await.unwrap().len(), 1);

            for (i, task) in [
                "Plan it", "Build it", "Build it", "Test it", "Ship it", "Tidy up", "Rest",
            ]
            .into_iter()
            .enumerate()
            {
                let mut t = turn("scout", Some("u1"), "2026-10-29T08:00:00Z");
                t.started_at += chrono::Duration::minutes(i as i64);
                t.task = Some(task.into());
                store.apply(&begin(&t, false)).await.unwrap();
            }
            assert_eq!(
                store.asks(Some("u1"), &scout).await.unwrap(),
                ["Plan it", "Build it", "Test it", "Ship it", "Tidy up", "Rest"],
                "its first asks and its latest, in order, once each"
            );

            let paragraph = |text: &str, when: &str| Paragraph {
                text: text.into(),
                at: at(when),
            };
            store
                .put_summary("u1", true, &paragraph("Now.", "2026-10-29T09:00:00Z"), 7)
                .await
                .unwrap();
            store
                .put_summary("u1", true, &paragraph("Before.", "2026-10-29T08:00:00Z"), 6)
                .await
                .unwrap();
            assert_eq!(
                store.summary("u1", true).await.unwrap(),
                Some((paragraph("Now.", "2026-10-29T09:00:00Z"), 7))
            );
            assert_eq!(store.summary("u1", false).await.unwrap(), None);

            store
                .put_gist(Some("u3"), &scout, &gist("Kept.", "2026-10-29T07:00:00Z"))
                .await
                .unwrap();
            store.reassign("u1", "u3").await.unwrap();
            assert_eq!(
                store.gist(Some("u3"), &scout).await.unwrap().map(|g| g.now),
                Some("Kept.".into()),
                "where both had one, the one moved into stays"
            );
            assert_eq!(store.summary("u1", true).await.unwrap(), None);
            assert_eq!(store.forget("u3").await.unwrap(), 8);
            assert_eq!(store.gist(Some("u3"), &scout).await.unwrap(), None);
            assert_eq!(store.prune(at("2026-10-30T00:00:00Z")).await.unwrap(), 2);
            assert!(store.gists(&personal).await.unwrap().is_empty());
        })
        .await;
    }
}
