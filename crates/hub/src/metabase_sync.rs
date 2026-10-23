//! Keeping each Metabase instance's published tool schema current, on a timer.
//!
//! The in-request path learns only from responses a turn already paid for, so a
//! cold process guesses once per database before it knows anything. The full
//! pass fills the store ahead of the first query and keeps it current as
//! schemas move; the reconcile tick between passes flushes what live clients
//! learned and gives a client that has never published its first text. Nothing
//! reaches a tool definition except through a publish here, which is what keeps
//! the prompt-cache prefix still between rounds.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tracing::info;
use workforce_common::metabase_schema::MetabaseSchemaStore;
use workforce_common::settings::WorkforceConfig;
use workforce_skills::adapters::metabase::client::MetabaseClient;
use workforce_skills::adapters::metabase::sync::{
    dedupe_by_site_url, publish_live_clients, sync_instance, SyncCaps, SyncReport,
};

use crate::agent_registry::AgentRegistry;

/// How often live clients flush usage and never-published clients publish.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

/// The bounds for a pass, or `None` when the pass should not run at all.
fn caps_if_enabled(config: &WorkforceConfig) -> Option<SyncCaps> {
    let metabase = &config.skills.metabase;
    metabase.schema_sync.then(|| {
        let (tables_with_columns, max_requests) = metabase.caps();
        SyncCaps {
            tables_with_columns,
            max_requests,
        }
    })
}

/// A full pass is due at start and once per interval thereafter.
fn full_pass_due(last_full: Option<Instant>, interval: Duration, now: Instant) -> bool {
    last_full.is_none_or(|at| now.duration_since(at) >= interval)
}

/// Spawn the loop: a reconcile every [`RECONCILE_INTERVAL`], and a full pass —
/// leader fetch when enabled and a store exists, then every live client
/// reloading from the store and publishing — once per configured interval.
pub fn spawn(
    registry: Arc<AgentRegistry>,
    config: Arc<WorkforceConfig>,
    store: Option<Arc<dyn MetabaseSchemaStore>>,
) -> JoinHandle<()> {
    let caps = caps_if_enabled(&config);
    if caps.is_none() {
        info!("Metabase schema sync disabled by config");
    }
    if store.is_none() {
        info!("Metabase schema sync inactive: no Postgres schema store; publishing in-request learning only");
    }
    let interval = config.skills.metabase.interval();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
        // The fetch and the publish keep separate clocks. A fetch this
        // replica could not run — no leader yet at startup, or another
        // replica leads — is tried again next tick, so a restart does not
        // leave the store unrefreshed for a whole interval. Publishing stays
        // on the interval; a client whose schema is still empty reloads every
        // tick until the store has something for it.
        let mut last_fetch: Option<Instant> = None;
        let mut last_publish: Option<Instant> = None;
        loop {
            tick.tick().await;
            publish_live_clients(false).await;
            let now = Instant::now();
            let mut fetched = false;
            if full_pass_due(last_fetch, interval, now) {
                fetched = match (store.as_ref(), caps) {
                    (Some(store), Some(caps)) => {
                        let ran = fetch_pass(&registry, store, caps).await;
                        if ran {
                            crate::know_how::stale::after_schema_sync(&config).await;
                        }
                        ran
                    }
                    _ => true,
                };
                if fetched {
                    last_fetch = Some(now);
                }
            }
            if fetched || full_pass_due(last_publish, interval, now) {
                let published = publish_live_clients(true).await;
                info!(
                    clients = published,
                    "Metabase schema published to live clients"
                );
                last_publish = Some(now);
            }
        }
    })
}

/// One fetch over every Metabase an agent can reach, into the store.
///
/// Leader-gated: the store is shared, so one syncer per fleet suffices and
/// several would multiply the request count for identical results. Services
/// share one leader flag, so asking any of them asks the fleet. Should a
/// follower ever run a pass anyway the damage is bounded — the fetches are
/// reads and the writes are idempotent upserts; usage counters, the one
/// additive write, are not touched here.
/// Whether this replica leads the fleet, so the fetch is its to run.
fn leads(registry: &AgentRegistry) -> bool {
    registry
        .all_services()
        .iter()
        .any(|s| s.leader_gate.is_leader())
}

/// `true` when the fetch ran; `false` when this replica does not lead.
async fn fetch_pass(
    registry: &AgentRegistry,
    store: &Arc<dyn MetabaseSchemaStore>,
    caps: SyncCaps,
) -> bool {
    if !leads(registry) {
        return false;
    }
    let services = registry.all_services();

    let candidates: Vec<(String, Arc<MetabaseClient>)> = services
        .iter()
        .filter_map(|service| service.vault.clone())
        .flat_map(|vault| crate::skill_loader::metabase_clients_for(vault, None))
        .collect();
    let targets = dedupe_by_site_url(candidates);

    let mut report = SyncReport::default();
    for (_, client) in &targets {
        report = report.merge(sync_instance(client, store, caps).await);
    }

    info!(
        agents = services.len(),
        instances = targets.len(),
        databases = report.databases,
        tables = report.tables,
        columns_fetched = report.columns_fetched,
        requests = report.requests,
        errors = report.errors,
        truncated = report.truncated,
        "Metabase schema sync pass complete"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(schema_sync: bool, tables: usize, requests: usize) -> WorkforceConfig {
        let mut config = WorkforceConfig::default();
        config.skills.metabase.schema_sync = schema_sync;
        config.skills.metabase.tables_with_columns = tables;
        config.skills.metabase.max_requests_per_pass = requests;
        config
    }

    #[test]
    fn disabled_config_yields_no_caps() {
        assert!(caps_if_enabled(&config_with(false, 25, 200)).is_none());
    }

    #[test]
    fn enabled_config_yields_validated_caps() {
        let caps = caps_if_enabled(&config_with(true, 25, 200)).expect("enabled");
        assert_eq!(caps.tables_with_columns, 25);
        assert_eq!(caps.max_requests, 200);
    }

    #[test]
    fn a_budget_below_the_table_count_is_corrected_upwards() {
        let caps = caps_if_enabled(&config_with(true, 25, 1)).expect("enabled");
        assert!(
            caps.max_requests > caps.tables_with_columns,
            "a pass must afford the metadata call as well as the tables it ranks"
        );
    }

    #[test]
    fn a_full_pass_is_due_at_start_and_after_the_interval() {
        let interval = Duration::from_secs(3_600);
        let started = Instant::now();
        assert!(full_pass_due(None, interval, started), "first tick");
        assert!(!full_pass_due(Some(started), interval, started), "just ran");
        assert!(
            !full_pass_due(Some(started), interval, started + interval / 2),
            "half an interval is too soon"
        );
        assert!(
            full_pass_due(Some(started), interval, started + interval),
            "an interval has elapsed"
        );
    }
}
