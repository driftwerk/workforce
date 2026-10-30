//! The floor's tables in Postgres, schema `floor`.

use std::collections::HashSet;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::GenericClient;
use eyre::{Context, Result};
use tokio_postgres::Row;
use uuid::Uuid;
use workforce_common::usage::{LoginRef, PlanKind, Reading};
use workforce_desk::floor::{self, Outcome, Scope, Settling, Target, Turn, TurnOp};
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

    /// Turns of `scope` started since `since`, every open one, and every
    /// question still unanswered, however old.
    pub async fn relevant(&self, scope: &Scope, since: DateTime<Utc>) -> Result<Vec<Turn>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM turns \
                     WHERE (owner = $1 OR ($2 AND owner IS NULL)) \
                       AND (started_at >= $3 OR ended_at IS NULL \
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

    /// Which of `instances` have crashed: their heartbeat went stale. One
    /// with no heartbeat is not known to have.
    pub async fn stale(&self, instances: &[String]) -> Result<HashSet<String>> {
        if instances.is_empty() {
            return Ok(HashSet::new());
        }
        let conn = self.pool.get().await?;
        let asked = conn
            .query(
                "SELECT instance_id FROM public.instance_heartbeats \
                 WHERE instance_id = ANY($1) \
                   AND last_seen < now() - make_interval(secs => $2::double precision)",
                &[
                    &instances,
                    &(workforce_common::session_postgres::HEARTBEAT_STALE_SECS as f64),
                ],
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

    /// Removes turns started before `before`, except questions still
    /// waiting on the person, and readings older than it; returns how many
    /// went.
    pub async fn prune(&self, before: DateTime<Utc>) -> Result<u64> {
        let conn = self.pool.get().await?;
        let mut removed = conn
            .execute(
                "DELETE FROM plan_readings WHERE observed_at < $1",
                &[&before],
            )
            .await?;
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

async fn settle(db: &impl GenericClient, s: &Settling) -> Result<()> {
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
}
