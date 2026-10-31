//! The install's decision record store: opened once and kept pruned, and
//! the per-person operations an identity's lifecycle runs on it. What is
//! recorded, and the recorder that writes it, is `decision_records`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use eyre::Result;
use tracing::warn;
use workforce_common::settings::WorkforceConfig;

use crate::hub_core::decision_records::{record_with, DecisionRecords, Recorder};

/// How often a running hub prunes records past their retention.
const PRUNE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Open the install's record store once and keep it pruned. Without
/// Postgres nothing is opened. With `[decision]` off, or records switched
/// off for the install, nothing is recorded and no schema is created, but
/// what an earlier configuration recorded is still pruned.
pub async fn install(config: &WorkforceConfig) {
    static OPENED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    OPENED
        .get_or_init(|| async {
            let records = config.decision.records;
            let recording = config.decision.enabled && records.enabled;
            let store = match open(config, recording).await {
                Ok(Some(store)) => store,
                Ok(None) => return,
                Err(e) => {
                    warn!(
                        target: "decision",
                        error = %format!("{e:#}"),
                        "decision record store unavailable; decisions are not recorded"
                    );
                    return;
                }
            };
            if recording {
                record_with(Recorder::start(Arc::clone(&store)));
            }
            if records.retention_days > 0 {
                tokio::spawn(prune_forever(store, records.retention_days));
            }
        })
        .await;
}

/// The install's store: created when `create`, else only when it is there
/// already. `None` without Postgres.
async fn open(config: &WorkforceConfig, create: bool) -> Result<Option<Arc<dyn DecisionRecords>>> {
    #[cfg(feature = "postgres")]
    if let Some(pg) = config.postgres.as_ref().filter(|p| p.enabled) {
        let store = match create {
            true => Some(postgres::PostgresDecisionRecords::new(pg).await?),
            false => postgres::PostgresDecisionRecords::existing(pg).await?,
        };
        return Ok(store.map(|store| Arc::new(store) as Arc<dyn DecisionRecords>));
    }
    let _ = (config, create);
    Ok(None)
}

/// Remove everything recorded for `person` under any of `names` (their
/// identity, and the channel ids it was linked to); returns how many rows
/// went. The store is looked for now, not as it was at boot, so records
/// kept before recording was switched off, or by another replica while this
/// one could not reach the store, go too. An error means nothing can be
/// said to have been erased.
/// How many records and interventions name any of `names`.
pub async fn person_records(config: &WorkforceConfig, names: &[String]) -> Result<u64> {
    let Some(store) = open(config, false).await? else {
        return Ok(0);
    };
    let mut held = 0;
    for name in names {
        held += store.person_records(name).await?;
    }
    Ok(held)
}

/// How many records and interventions name each person.
pub async fn people(config: &WorkforceConfig) -> Result<BTreeMap<String, u64>> {
    match open(config, false).await? {
        Some(store) => store.people().await,
        None => Ok(BTreeMap::new()),
    }
}

/// Make every record of anyone in `names` name `to` (one identity merged
/// into another); returns how many changed.
pub async fn reassign_person(config: &WorkforceConfig, names: &[String], to: &str) -> Result<u64> {
    let Some(store) = open(config, false).await? else {
        return Ok(0);
    };
    let mut moved = 0;
    for name in names.iter().filter(|name| name.as_str() != to) {
        moved += store.reassign_person(name, to).await?;
    }
    Ok(moved)
}

pub async fn forget_person(config: &WorkforceConfig, names: &[String]) -> Result<u64> {
    let Some(store) = open(config, false).await? else {
        return Ok(0);
    };
    let mut removed = 0;
    for name in names {
        removed += store.forget_person(name).await?;
    }
    Ok(removed)
}

async fn prune_forever(store: Arc<dyn DecisionRecords>, retention_days: u32) {
    let mut tick = tokio::time::interval(PRUNE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let kept = chrono::Duration::days(i64::from(retention_days));
        let Some(before) = Utc::now().checked_sub_signed(kept) else {
            continue;
        };
        match store.prune(before).await {
            Ok(0) => {}
            Ok(removed) => {
                tracing::info!(target: "decision", removed, retention_days, "pruned decision records")
            }
            Err(e) => {
                warn!(target: "decision", error = %format!("{e:#}"), "pruning decision records failed")
            }
        }
    }
}

#[cfg(feature = "postgres")]
pub mod postgres {
    use super::*;
    use async_trait::async_trait;
    use chrono::DateTime;
    use workforce_orchestrator::workflow::instance::Intervention;

    use crate::hub_core::decision_records::{person_of, NewRecord, Record};
    use eyre::Context;
    use workforce_postgres::{
        MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
    };

    // Own directory, never a subdir of another embed root — refinery's
    // embed_migrations is recursive.
    mod embedded {
        refinery::embed_migrations!("migrations_decision");
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

    pub struct PostgresDecisionRecords {
        pool: PostgresPool,
    }

    const SEARCH_PATH: &str = "SET search_path TO decision, public";

    /// Rows one prune statement removes, so no delete holds its locks for
    /// long.
    const PRUNE_BATCH: i64 = 5_000;

    /// The tables a prune and an erasure cover.
    const TABLES: [&str; 2] = ["records", "interventions"];

    impl PostgresDecisionRecords {
        pub const SCHEMA: &'static str = "decision";

        pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
            PostgresPool::from_config_with_session_init(&config.small(), &[SEARCH_PATH])
        }

        pub async fn new(config: &PostgresConfig) -> Result<Self> {
            let pool = Self::build_pool(config).context("building decision records pool")?;
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
            .context("running decision record migrations")?;
            Ok(Self { pool })
        }

        /// The store when its table is already there, without creating
        /// anything: for erasing and pruning what an install recorded
        /// before it switched recording off.
        pub async fn existing(config: &PostgresConfig) -> Result<Option<Self>> {
            let pool = Self::build_pool(config).context("building decision records pool")?;
            let conn = pool.get().await?;
            let row = conn
                .query_one(
                    "SELECT to_regclass('decision.records') IS NOT NULL \
                        AND to_regclass('decision.interventions') IS NOT NULL",
                    &[],
                )
                .await?;
            drop(conn);
            Ok(row.get::<_, bool>(0).then_some(Self { pool }))
        }

        /// The store as an offline reader opens it: no migration is run and
        /// every transaction is read-only, so a newer binary on the host
        /// can neither change the schema under a running hub nor write.
        pub fn read_only(config: &PostgresConfig) -> Result<Self> {
            let pool = PostgresPool::from_config_with_session_init(
                config,
                &[SEARCH_PATH, workforce_postgres::READ_ONLY_SESSION],
            )
            .context("building read-only decision records pool")?;
            Ok(Self { pool })
        }
    }

    fn record(row: &tokio_postgres::Row) -> Result<Record> {
        Ok(Record {
            id: row.get("id"),
            at: row.get("at"),
            record: NewRecord {
                usage: row.get("usage"),
                agent_id: row.get("agent_id"),
                person: row.get("person"),
                channel: row.get("channel"),
                instance_id: row.get("instance_id"),
                step_id: row.get("step_id"),
                input: serde_json::from_value(row.get("input"))?,
                request_chars: row.get::<_, i32>("request_chars").max(0) as usize,
                backend: row.get("backend"),
                model: row.get("model"),
                answers: serde_json::from_value(row.get("answers"))?,
                acted: row.get("acted"),
                verdict: row.get("verdict"),
            },
        })
    }

    #[async_trait]
    impl DecisionRecords for PostgresDecisionRecords {
        async fn insert(&self, record: NewRecord) -> Result<i64> {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "INSERT INTO records (usage, agent_id, person, channel, instance_id, step_id, \
                         input, request_chars, backend, model, answers, acted, verdict) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
                     RETURNING id",
                    &[
                        &record.usage,
                        &record.agent_id,
                        &record.person,
                        &record.channel,
                        &record.instance_id,
                        &record.step_id,
                        &serde_json::to_value(&record.input)?,
                        &i32::try_from(record.request_chars).unwrap_or(i32::MAX),
                        &record.backend,
                        &record.model,
                        &serde_json::to_value(&record.answers)?,
                        &record.acted,
                        &record.verdict,
                    ],
                )
                .await?;
            Ok(row.get(0))
        }

        async fn list(
            &self,
            usage: &str,
            since: DateTime<Utc>,
            after: i64,
            limit: usize,
        ) -> Result<Vec<Record>> {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT id, at, usage, agent_id, person, channel, instance_id, step_id, \
                            input, request_chars, backend, model, answers, acted, verdict \
                     FROM records WHERE usage = $1 AND at >= $2 AND id > $3 \
                     ORDER BY id LIMIT $4",
                    &[
                        &usage,
                        &since,
                        &after,
                        &i64::try_from(limit).unwrap_or(i64::MAX),
                    ],
                )
                .await?;
            rows.iter().map(record).collect()
        }

        async fn insert_intervention(
            &self,
            instance_id: &str,
            intervention: Intervention,
        ) -> Result<()> {
            let conn = self.pool.get().await?;
            conn.execute(
                "INSERT INTO interventions (at, instance_id, person, intervention) \
                 VALUES ($1, $2, $3, $4)",
                &[
                    &intervention.at,
                    &instance_id,
                    &person_of(&intervention),
                    &serde_json::to_value(&intervention)?,
                ],
            )
            .await?;
            Ok(())
        }

        async fn interventions(
            &self,
            instance_ids: &[String],
        ) -> Result<BTreeMap<String, Vec<Intervention>>> {
            let conn = self.pool.get().await?;
            let asked = conn
                .query(
                    "SELECT instance_id, intervention FROM interventions \
                     WHERE instance_id = ANY($1) ORDER BY at, id",
                    &[&instance_ids],
                )
                .await;
            // An install that never recorded has no table: no run was moved
            // where anything kept it.
            let rows = match asked {
                Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) => {
                    Vec::new()
                }
                asked => asked?,
            };
            let mut found: BTreeMap<String, Vec<Intervention>> = BTreeMap::new();
            for row in rows {
                // One this binary cannot read was written by a newer hub.
                if let Ok(intervention) = serde_json::from_value(row.get("intervention")) {
                    let kept = found.entry(row.get("instance_id")).or_default();
                    kept.push(intervention);
                }
            }
            Ok(found)
        }

        async fn prune(&self, before: DateTime<Utc>) -> Result<u64> {
            let conn = self.pool.get().await?;
            let mut removed = 0;
            for table in TABLES {
                let delete = format!(
                    "DELETE FROM {table} WHERE id IN \
                         (SELECT id FROM {table} WHERE at < $1 ORDER BY at LIMIT $2)"
                );
                loop {
                    let batch = conn.execute(&delete, &[&before, &PRUNE_BATCH]).await?;
                    removed += batch;
                    if batch < PRUNE_BATCH as u64 {
                        break;
                    }
                }
            }
            Ok(removed)
        }

        async fn person_records(&self, person: &str) -> Result<u64> {
            let conn = self.pool.get().await?;
            let row = conn
                .query_one(
                    "SELECT (SELECT COUNT(*) FROM records WHERE person = $1)
                          + (SELECT COUNT(*) FROM interventions WHERE person = $1)",
                    &[&person],
                )
                .await?;
            Ok(u64::try_from(row.get::<_, i64>(0)).unwrap_or_default())
        }

        async fn people(&self) -> Result<BTreeMap<String, u64>> {
            let conn = self.pool.get().await?;
            let rows = conn
                .query(
                    "SELECT person, COUNT(*) FROM ( \
                         SELECT person FROM records WHERE person IS NOT NULL \
                         UNION ALL SELECT person FROM interventions WHERE person IS NOT NULL \
                     ) AS named GROUP BY person",
                    &[],
                )
                .await?;
            Ok(rows
                .iter()
                .map(|row| {
                    (
                        row.get::<_, String>(0),
                        u64::try_from(row.get::<_, i64>(1)).unwrap_or_default(),
                    )
                })
                .collect())
        }

        async fn reassign_person(&self, from: &str, to: &str) -> Result<u64> {
            let mut conn = self.pool.get().await?;
            let tx = conn.transaction().await?;
            let records = tx
                .execute(
                    "UPDATE records SET person = $2 WHERE person = $1",
                    &[&from, &to],
                )
                .await?;
            let interventions = tx
                .execute(
                    "UPDATE interventions
                     SET person = $2, intervention = jsonb_set(intervention, '{actor,id}', to_jsonb($2::text))
                     WHERE person = $1",
                    &[&from, &to],
                )
                .await?;
            tx.commit().await?;
            Ok(records + interventions)
        }

        async fn forget_person(&self, person: &str) -> Result<u64> {
            let conn = self.pool.get().await?;
            let mut removed = 0;
            for table in TABLES {
                let delete = format!(
                    "DELETE FROM {table} WHERE id IN \
                         (SELECT id FROM {table} WHERE person = $1 LIMIT $2)"
                );
                loop {
                    let batch = conn.execute(&delete, &[&person, &PRUNE_BATCH]).await?;
                    removed += batch;
                    if batch < PRUNE_BATCH as u64 {
                        break;
                    }
                }
            }
            Ok(removed)
        }
    }
}

#[cfg(all(test, feature = "postgres"))]
mod tests {
    use super::*;
    use serde_json::json;
    use workforce_decide::{Input, RequestBudget};

    use crate::hub_core::decision_records::testing::*;
    use crate::hub_core::decision_records::{person_of, NewRecord, Pending};

    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_reassigns_a_person_records_and_interventions_alike() {
        use super::postgres::PostgresDecisionRecords;
        workforce_postgres::test_support::with_test_pool(
            PostgresDecisionRecords::build_pool,
            |pool| async move {
                let store = PostgresDecisionRecords::with_pool(pool)
                    .await
                    .expect("store");
                reassign_person_contract(&store).await;
            },
        )
        .await;
    }

    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_keeps_a_decide_first_record_with_its_never_floor_and_the_agents_label() {
        use super::postgres::PostgresDecisionRecords;
        use workforce_decide::first::{self, Floors};
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reached = Arc::clone(&ran);
        workforce_postgres::test_support::with_test_pool(
            PostgresDecisionRecords::build_pool,
            |pool| async move {
                let store = PostgresDecisionRecords::with_pool(pool)
                    .await
                    .expect("store");
                let input = Input::DecideFirst {
                    state: r#"{"subject": "Weekly digest \u{1F4EC}"}"#.into(),
                    questions: first::questions(
                        None,
                        "A crash: OUTCOME: bug",
                        &["bug".into()],
                        &BTreeMap::from([("ignore".into(), "A newsletter".into())]),
                        true,
                    ),
                    floors: Floors {
                        min_probability: 0.9,
                        options: BTreeMap::from([("bug".into(), None)]),
                    },
                };
                let asked = decision(json!({
                    first::QUESTION_ID: {"type": "choice", "choice": "ignore",
                        "probabilities": {"ignore": 0.93, "bug": 0.07}, "confidence": 0.9},
                }));
                let mut records =
                    Pending::of(&recorded(), &input, RequestBudget::DEFAULT, &asked, false)
                        .into_records();
                let new = NewRecord {
                    verdict: Some("ignore".into()),
                    instance_id: Some("i-1".into()),
                    step_id: Some("classify".into()),
                    ..records.remove(0)
                };
                let before = Utc::now() - chrono::Duration::seconds(5);
                let id = store.insert(new.clone()).await.unwrap();
                let listed = store.list("decide_first", before, 0, 10).await.unwrap();
                assert_eq!(listed.len(), 1);
                assert_eq!((listed[0].id, &listed[0].record), (id, &new));
                assert_eq!(
                    crate::decide::would_settle(&listed[0].record.input, &listed[0].record.answers),
                    Some(("ignore".to_string(), 0.93)),
                    "read back, the record still says what the model would have settled"
                );
                reached.store(true, std::sync::atomic::Ordering::SeqCst);
            },
        )
        .await;
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst)
                || std::env::var("WORKFORCE_SKIP_POSTGRES_TESTS").is_ok(),
            "the Postgres test body ran"
        );
    }

    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_round_trips_a_record_lists_by_use_and_since_and_prunes_in_batches() {
        use super::postgres::PostgresDecisionRecords;
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reached = Arc::clone(&ran);
        workforce_postgres::test_support::with_test_pool(
            PostgresDecisionRecords::build_pool,
            |pool| async move {
                let store = PostgresDecisionRecords::with_pool(pool.clone())
                    .await
                    .expect("store");
                PostgresDecisionRecords::with_pool(pool.clone())
                    .await
                    .expect("the migration is idempotent");
                let input = scope("Roll back the deploy? It is \u{1F525}");
                let mut records = Pending::of(
                    &recorded(),
                    &input,
                    RequestBudget::chars(900),
                    &noul(0.9),
                    false,
                )
                .into_records();
                let new = NewRecord {
                    verdict: Some("off_topic".into()),
                    instance_id: Some("i-1".into()),
                    step_id: Some("triage".into()),
                    ..records.remove(0)
                };
                let before = Utc::now() - chrono::Duration::seconds(5);
                let id = store.insert(new.clone()).await.unwrap();
                let listed = store.list("scope_gate", before, 0, 10).await.unwrap();
                assert_eq!(listed.len(), 1);
                assert_eq!((listed[0].id, &listed[0].record), (id, &new));
                assert!(store
                    .list("routing", before, 0, 10)
                    .await
                    .unwrap()
                    .is_empty());
                let later = Utc::now() + chrono::Duration::seconds(5);
                assert!(store
                    .list("scope_gate", later, 0, 10)
                    .await
                    .unwrap()
                    .is_empty());
                let second = store.insert(new.clone()).await.unwrap();
                assert_eq!(
                    store.list("scope_gate", before, 0, 1).await.unwrap().len(),
                    1
                );
                let next = store.list("scope_gate", before, id, 10).await.unwrap();
                assert_eq!(
                    next.iter().map(|r| r.id).collect::<Vec<_>>(),
                    [second],
                    "the page after the first record"
                );

                let bob = NewRecord {
                    person: Some("bob".into()),
                    ..new.clone()
                };
                store.insert(bob).await.unwrap();
                store
                    .insert_intervention("i-1", moved_by("bob"))
                    .await
                    .unwrap();
                store
                    .insert_intervention("i-1", moved_by("carol"))
                    .await
                    .unwrap();
                store
                    .insert_intervention("i-2", moved_by("carol"))
                    .await
                    .unwrap();
                let runs = ["i-1".to_string(), "i-3".to_string()];
                let found = store.interventions(&runs).await.unwrap();
                assert_eq!(found.len(), 1);
                assert_eq!(
                    found["i-1"].iter().map(person_of).collect::<Vec<_>>(),
                    [Some("bob"), Some("carol")],
                    "a run's interventions, oldest first"
                );
                assert_eq!(found["i-1"][0].to_step.as_deref(), Some("fix"));

                assert_eq!(
                    store.forget_person("bob").await.unwrap(),
                    2,
                    "their record and their intervention"
                );
                assert_eq!(store.forget_person("nobody").await.unwrap(), 0);
                let found = store.interventions(&runs).await.unwrap();
                assert_eq!(found["i-1"].len(), 1);

                assert_eq!(store.prune(before).await.unwrap(), 0);
                assert_eq!(store.prune(later).await.unwrap(), 4);
                assert!(store.interventions(&runs).await.unwrap().is_empty());
                assert!(store
                    .list("scope_gate", before, 0, 10)
                    .await
                    .unwrap()
                    .is_empty());
                reached.store(true, std::sync::atomic::Ordering::SeqCst);
            },
        )
        .await;
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst)
                || std::env::var("WORKFORCE_SKIP_POSTGRES_TESTS").is_ok(),
            "the Postgres test body ran"
        );
    }
}
