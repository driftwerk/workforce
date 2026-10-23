//! Retention for the stores that would otherwise grow forever: a task's
//! telemetry (`[telemetry] activity_retention_days`) and cost records
//! (`[telemetry] cost_retention_days`), both kept forever unless the
//! install sets a window, and resolved routed approvals (`[approvals]
//! retention_days`), and shadow mode's comparisons and their daily
//! rollup (DW-676), when a store is shadowed.
//!
//! One task per hub prunes them all, off the hot path, every
//! [`PRUNE_INTERVAL`] from [`BOOT_SINGLETON_DELAY`] after boot. In a
//! cluster only the leader prunes. Each store deletes in short batches that
//! skip rows another writer holds, and a pass stops after a bounded number
//! of them, so neither a busy Postgres install nor a SQLite one waits on a
//! long lock, even on the first pass over years of rows.
//!
//! [`BOOT_SINGLETON_DELAY`]: crate::leader_gate::BOOT_SINGLETON_DELAY

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use eyre::Result;
use tokio_util::sync::CancellationToken;
use workforce_common::settings::WorkforceConfig;
use workforce_common::shadow_postgres::{
    ShadowStore, COMPARISONS_RETENTION_DAYS, DAILY_RETENTION_DAYS, RECONCILES_RETENTION_DAYS,
};
use workforce_common::telemetry::TelemetryStore;

use crate::approvals::PendingApprovalStore;

/// How often the hub prunes.
const PRUNE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

type Pass = Arc<dyn Fn(DateTime<Utc>) -> Result<usize> + Send + Sync>;

/// One store's prune: what it removes everything older than `retention_days`
/// of with.
#[derive(Clone)]
pub struct Prune {
    name: &'static str,
    retention_days: u32,
    pass: Pass,
}

impl Prune {
    fn new(
        name: &'static str,
        retention_days: u32,
        pass: impl Fn(DateTime<Utc>) -> Result<usize> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name,
            retention_days,
            pass: Arc::new(pass),
        }
    }
}

/// The prunes the install's configuration asks for, over `telemetry` and,
/// when routed approvals are in use, their store, and when a store is
/// shadowed, the shadow tables.
pub fn prunes(
    config: &WorkforceConfig,
    telemetry: Arc<dyn TelemetryStore>,
    approvals: Option<Arc<dyn PendingApprovalStore>>,
    shadow: Option<Arc<ShadowStore>>,
) -> Vec<Prune> {
    let activity = Arc::clone(&telemetry);
    let mut prunes = vec![
        Prune::new(
            "task telemetry",
            config.telemetry.activity_retention_days,
            move |before| activity.prune_activity(before),
        ),
        Prune::new(
            "cost records",
            config.telemetry.cost_retention_days,
            move |before| telemetry.prune_costs(before),
        ),
    ];
    prunes.extend(approvals.map(|store| {
        Prune::new(
            "resolved approvals",
            config.approvals.retention_days,
            move |before| store.prune_resolved(before),
        )
    }));
    if let Some(shadow) = shadow {
        let daily = Arc::clone(&shadow);
        // A day is rolled up before its comparisons go.
        prunes.push(Prune::new(
            "shadow comparisons",
            COMPARISONS_RETENTION_DAYS,
            move |before| shadow.roll_up_and_prune_comparisons(before),
        ));
        let reconciles = Arc::clone(&daily);
        prunes.push(Prune::new(
            "shadow daily",
            DAILY_RETENTION_DAYS,
            move |before| daily.prune_daily(before),
        ));
        prunes.push(Prune::new(
            "shadow reconciles",
            RECONCILES_RETENTION_DAYS,
            move |before| reconciles.prune_reconciles(before),
        ));
    }
    prunes
}

/// Prune once the hub has settled after boot, then every
/// [`PRUNE_INTERVAL`], until `drain` is cancelled. A store kept forever
/// (`0` days) is never touched; with none to prune nothing is spawned.
pub fn spawn(prunes: Vec<Prune>, leader_flag: Option<Arc<AtomicBool>>, drain: CancellationToken) {
    let (kept, prunes): (Vec<_>, Vec<_>) = prunes.into_iter().partition(|p| p.retention_days == 0);
    for prune in &kept {
        tracing::info!(store = prune.name, "Retention off — kept forever");
    }
    if prunes.is_empty() {
        return;
    }
    tokio::spawn(async move {
        tokio::select! {
            _ = drain.cancelled() => return,
            _ = tokio::time::sleep(crate::leader_gate::BOOT_SINGLETON_DELAY) => {}
        }
        let mut tick = tokio::time::interval(PRUNE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = drain.cancelled() => return,
                _ = tick.tick() => {}
            }
            let leads = leader_flag
                .as_ref()
                .is_none_or(|flag| flag.load(Ordering::Relaxed));
            if leads {
                prune_once(&prunes, Utc::now()).await;
            }
        }
    });
}

/// Run every prune once, as of `now`, skipping a store kept forever. A
/// store's failure is logged and leaves the others to run.
async fn prune_once(prunes: &[Prune], now: DateTime<Utc>) {
    for prune in prunes.iter().filter(|p| p.retention_days > 0) {
        let kept = chrono::Duration::days(i64::from(prune.retention_days));
        let Some(before) = now.checked_sub_signed(kept) else {
            continue;
        };
        let pass = Arc::clone(&prune.pass);
        let (store, retention_days) = (prune.name, prune.retention_days);
        match tokio::task::spawn_blocking(move || pass(before)).await {
            Ok(Ok(0)) => {}
            Ok(Ok(removed)) => {
                tracing::info!(store, removed, retention_days, "Pruned past retention")
            }
            Ok(Err(e)) => tracing::warn!(store, error = %format!("{e:#}"), "Prune failed"),
            Err(e) => tracing::warn!(store, error = %e, "Prune task failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test(flavor = "multi_thread")]
    async fn each_store_is_pruned_to_its_own_window_and_a_failure_stops_no_other() {
        let asked: Arc<Mutex<Vec<(&str, DateTime<Utc>)>>> = Arc::default();
        let prune = |name: &'static str, days: u32, fails: bool| {
            let asked = Arc::clone(&asked);
            Prune::new(name, days, move |before| {
                asked.lock().unwrap().push((name, before));
                match fails {
                    true => Err(eyre::eyre!("store unreachable")),
                    false => Ok(3),
                }
            })
        };
        let now = Utc::now();
        prune_once(
            &[
                prune("failing", 1, true),
                prune("forever", 0, false),
                prune("costs", 30, false),
            ],
            now,
        )
        .await;
        assert_eq!(
            *asked.lock().unwrap(),
            [
                ("failing", now - chrono::Duration::days(1)),
                ("costs", now - chrono::Duration::days(30)),
            ]
        );
    }

    #[test]
    fn the_install_defaults_keep_telemetry_and_prune_resolved_approvals() {
        let config = WorkforceConfig::default();
        let telemetry: Arc<dyn TelemetryStore> =
            Arc::new(workforce_common::persistence::SqliteStore::in_memory().unwrap());
        let approvals: Arc<dyn PendingApprovalStore> =
            Arc::new(crate::approvals::SqlitePendingApprovalStore::in_memory().unwrap());
        let windows = |approvals| {
            prunes(&config, Arc::clone(&telemetry), approvals, None)
                .into_iter()
                .map(|p| (p.name, p.retention_days))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            windows(Some(approvals)),
            [
                ("task telemetry", 0),
                ("cost records", 0),
                ("resolved approvals", 30)
            ]
        );
        assert_eq!(
            windows(None).len(),
            2,
            "no approvals store, nothing to prune"
        );
    }

    /// A shadowed store's tables are kept to their windows by the leader's
    /// pass: a day rolled up before its comparisons go, after 14 days; the
    /// rollup after 180. With no store shadowed there is nothing to prune.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_shadow_tables_are_kept_to_their_windows_by_the_leaders_pass() {
        use workforce_common::telemetry_postgres::PostgresTelemetryStore;
        let ran = Arc::new(AtomicBool::new(false));
        let reached = Arc::clone(&ran);
        workforce_postgres::test_support::with_test_pool(
            PostgresTelemetryStore::build_pool,
            |pool| async move {
                let shadow = ShadowStore::with_pool(pool.clone()).await.unwrap();
                let config = WorkforceConfig::default();
                let telemetry: Arc<dyn TelemetryStore> =
                    Arc::new(workforce_common::persistence::SqliteStore::in_memory().unwrap());
                let windows = |shadow| {
                    prunes(&config, Arc::clone(&telemetry), None, shadow)
                        .into_iter()
                        .map(|p| (p.name, p.retention_days))
                        .collect::<Vec<_>>()
                };
                assert_eq!(
                    windows(Some(Arc::clone(&shadow)))[2..],
                    [
                        ("shadow comparisons", 14),
                        ("shadow daily", 180),
                        ("shadow reconciles", 180)
                    ]
                );
                assert_eq!(windows(None).len(), 2, "nothing shadowed, nothing to prune");

                let conn = pool.get().await.unwrap();
                let old = Utc::now() - chrono::Duration::days(20);
                conn.execute(
                    "INSERT INTO shadow_comparisons
                         (store, at, outcome, probe, primary_ms, primary_count, shadow_ms)
                     VALUES ('memories', $1, 'failed', false, 1, 0, 1),
                            ('memories', now(), 'failed', false, 1, 0, 1)",
                    &[&old],
                )
                .await
                .unwrap();
                let ancient = (Utc::now() - chrono::Duration::days(200)).date_naive();
                conn.execute(
                    "INSERT INTO shadow_daily (store, day, samples, answered, failed, panicked,
                         timed_out, probes, empty_primary_only, empty_shadow_only)
                     VALUES ('artifacts', $1, 1, 1, 0, 0, 0, 0, 0, 0)",
                    &[&ancient],
                )
                .await
                .unwrap();

                prune_once(&prunes(&config, telemetry, None, Some(shadow)), Utc::now()).await;
                let left: i64 = conn
                    .query_one("SELECT count(*) FROM shadow_comparisons", &[])
                    .await
                    .unwrap()
                    .get(0);
                assert_eq!(left, 1, "the 20-day-old comparison went, today's stayed");
                let days: Vec<(String, i32)> = conn
                    .query("SELECT store, samples FROM shadow_daily ORDER BY day", &[])
                    .await
                    .unwrap()
                    .iter()
                    .map(|r| (r.get(0), r.get(1)))
                    .collect();
                assert_eq!(
                    days,
                    [("memories".to_string(), 1)],
                    "its day rolled up before it went; the 200-day-old day gone"
                );
                reached.store(true, Ordering::SeqCst);
            },
        )
        .await;
        assert!(
            ran.load(Ordering::SeqCst) || std::env::var("WORKFORCE_SKIP_POSTGRES_TESTS").is_ok()
        );
    }

    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pass_prunes_a_postgres_store_from_the_blocking_pool() {
        use workforce_common::telemetry_postgres::PostgresTelemetryStore;
        let ran = Arc::new(AtomicBool::new(false));
        let reached = Arc::clone(&ran);
        workforce_postgres::test_support::with_test_pool(
            PostgresTelemetryStore::build_pool,
            |pool| async move {
                let store = PostgresTelemetryStore::with_pool(pool).await.unwrap();
                let task =
                    |id: &str, days: i64| workforce_common::persistence::TaskTelemetryRecord {
                        agent_id: "agent".into(),
                        model: "m".into(),
                        provider: "anthropic".into(),
                        input_tokens: 1,
                        output_tokens: 1,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        cost_usd: 0.0,
                        rounds: 0,
                        duration_ms: 1,
                        tool_invocation_count: 0,
                        success: true,
                        error_type: None,
                        retry_count: 0,
                        task_type: None,
                        task_id: Some(id.into()),
                        started_at: (Utc::now() - chrono::Duration::days(days)).to_rfc3339(),
                        completed_at: (Utc::now() - chrono::Duration::days(days)).to_rfc3339(),
                        forecast_cost_usd: None,
                        forecast_rounds: None,
                        forecast_confidence: None,
                        team_id: None,
                        session_id: None,
                        model_switches: None,
                        ticket_id: None,
                        identity_id: None,
                        description: None,
                    };
                for (id, days) in [("old", 500), ("recent", 1)] {
                    store
                        .record_task_telemetry(&task(id, days), &[], &[])
                        .unwrap();
                }
                let store: Arc<dyn TelemetryStore> = store;
                let mut config = WorkforceConfig::default();
                config.telemetry.activity_retention_days = 400;
                prune_once(&prunes(&config, Arc::clone(&store), None, None), Utc::now()).await;
                assert!(store.get_task_trace("old").unwrap().is_none());
                assert!(store.get_task_trace("recent").unwrap().is_some());
                reached.store(true, Ordering::SeqCst);
            },
        )
        .await;
        assert!(
            ran.load(Ordering::SeqCst) || std::env::var("WORKFORCE_SKIP_POSTGRES_TESTS").is_ok()
        );
    }
}
