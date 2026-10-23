//! Per-agent background-runner supervisors.
//!
//! Self-learning and heartbeat are per-agent features (per-agent config, plans
//! and checks under each agent's `service.workspace`, per-agent store), but were
//! historically spawned ONCE for the default agent against a hardcoded
//! `{data_dir}/workspace` — so on a multi-agent install every other agent's
//! plans/checks were written but never read, and even the default agent's were
//! read from the wrong directory when its workspace was not `{data_dir}/workspace`.
//!
//! A supervisor instead runs one runner per live agent, each scoped to that
//! agent's own `service.workspace`, and reconciles against registry membership
//! so agents added or removed at runtime gain or lose their runner with no
//! restart. Each runner's `CancellationToken` is a child of the supervisor's, so
//! a hub shutdown stops every runner.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono_tz::Tz;
use tokio_util::sync::CancellationToken;
use tracing::info;
use workforce_common::settings::HeartbeatSettings;

use crate::agent_registry::AgentRegistry;
use crate::heartbeat::{HeartbeatDeliveryCallback, HeartbeatRunner};
use crate::self_learning::SelfLearningRunner;
use crate::serve::AgentService;

/// How often a supervisor reconciles its runners against live registry
/// membership. A newly-spawned agent gains its runner within this window; a
/// removed one loses it.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Identity of a running service instance — the address of its `Arc<AgentService>`.
/// The config-overlay watcher replaces an agent in place (`remove(id)` then
/// `insert(id, new Arc)`) so the id alone is stable across a respawn; comparing
/// the instance identity catches the swap. A `usize` (not a raw pointer) keeps
/// the supervisor future `Send` and is only ever compared, never dereferenced.
type ServiceIdentity = usize;

fn identity(service: &Arc<AgentService>) -> ServiceIdentity {
    Arc::as_ptr(service) as ServiceIdentity
}

/// Pure reconciliation by `(id, instance identity)`: an id whose live instance
/// is gone or has changed must be `stop`ped; an id that is newly live or whose
/// instance changed must be `start`ed. A replaced id appears in both (stop the
/// stale runner, start one for the new instance).
fn reconcile(
    running: &HashMap<String, ServiceIdentity>,
    live: &HashMap<String, ServiceIdentity>,
) -> (Vec<String>, Vec<String>) {
    let stop = running
        .iter()
        .filter(|(id, ident)| live.get(*id) != Some(ident))
        .map(|(id, _)| id.clone())
        .collect();
    let start = live
        .iter()
        .filter(|(id, ident)| running.get(*id) != Some(ident))
        .map(|(id, _)| id.clone())
        .collect();
    (stop, start)
}

/// Drive one runner per live agent: spawn for agents that appear (or are
/// replaced in place), cancel for agents that leave or are replaced.
/// `spawn_one(service, child)` builds and spawns a runner bound to `child` (a
/// child token of `cancel`, so a hub shutdown stops every runner). Runs until
/// `cancel` fires.
async fn supervise<F>(
    registry: Weak<AgentRegistry>,
    cancel: CancellationToken,
    label: &str,
    mut spawn_one: F,
) where
    F: FnMut(&Arc<AgentService>, CancellationToken),
{
    let mut running: HashMap<String, (CancellationToken, ServiceIdentity)> = HashMap::new();
    loop {
        if let Some(reg) = registry.upgrade() {
            let services = reg.all_services();
            let live: HashMap<String, ServiceIdentity> = services
                .iter()
                .map(|s| (s.agent_id.as_str().to_string(), identity(s)))
                .collect();
            let running_idents: HashMap<String, ServiceIdentity> = running
                .iter()
                .map(|(id, (_, ident))| (id.clone(), *ident))
                .collect();
            let (stop, start) = reconcile(&running_idents, &live);

            for id in stop {
                if let Some((child, _)) = running.remove(&id) {
                    child.cancel();
                    info!(agent = %id, "{} runner stopped", label);
                }
            }
            for service in &services {
                let id = service.agent_id.as_str();
                if start.iter().any(|s| s == id) {
                    let child = cancel.child_token();
                    spawn_one(service, child.clone());
                    running.insert(id.to_string(), (child, identity(service)));
                    info!(agent = %id, "{} runner started", label);
                }
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(RECONCILE_INTERVAL) => {}
        }
    }
}

/// Run one self-learning runner per live agent, each curating from its own
/// `service.workspace`.
pub async fn supervise_self_learning(
    registry: Weak<AgentRegistry>,
    leader_flag: Option<Arc<AtomicBool>>,
    user_tz: Tz,
    cancel: CancellationToken,
) {
    supervise(registry, cancel, "Self-learning", |service, child| {
        let (mut runner, wake) =
            SelfLearningRunner::new(Arc::clone(service), &service.workspace, user_tz);
        if let Some(flag) = &leader_flag {
            runner.set_leader_flag(Arc::clone(flag));
        }
        service.set_self_learning_handle(Arc::new(wake));
        tokio::spawn(async move { runner.run(child).await });
    })
    .await;
}

/// Run one heartbeat runner per live agent, each reading checks from its own
/// `service.workspace`.
pub async fn supervise_heartbeat(
    config: HeartbeatSettings,
    registry: Weak<AgentRegistry>,
    leader_flag: Option<Arc<AtomicBool>>,
    user_tz: Tz,
    delivery: Option<HeartbeatDeliveryCallback>,
    cancel: CancellationToken,
) {
    supervise(registry, cancel, "Heartbeat", |service, child| {
        let (mut runner, wake) = HeartbeatRunner::new(
            config.clone(),
            Arc::clone(service),
            &service.workspace,
            user_tz,
            delivery.clone(),
        );
        if let Some(flag) = &leader_flag {
            runner.set_leader_flag(Arc::clone(flag));
        }
        service.set_heartbeat_handle(wake);
        tokio::spawn(async move { runner.run(child).await });
    })
    .await;
}

/// Run each agent's scheduler tick loop, one per live agent that owns a
/// scheduler. Membership reconciliation (identity-aware by `Arc::as_ptr`) means
/// a config-overlay respawn cancels the stale agent's loop and starts the new
/// one, and a removed agent's loop is cancelled — so the tick loop never leaks
/// or runs jobs for a dropped service. `AgentRegistry::remove` retires the
/// scheduler first; this reconcile is the backstop. The scheduler itself is
/// built during agent construction; this only drives its loop.
pub async fn supervise_scheduler(registry: Weak<AgentRegistry>, cancel: CancellationToken) {
    supervise(registry, cancel, "Scheduler", |service, child| {
        if let Some(scheduler) = service.scheduler.clone() {
            tokio::spawn(async move { scheduler.run(child).await });
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(xs: &[(&str, ServiceIdentity)]) -> HashMap<String, ServiceIdentity> {
        xs.iter().map(|(id, i)| (id.to_string(), *i)).collect()
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    #[test]
    fn new_agents_start_departed_agents_stop() {
        let (stop, start) = reconcile(&map(&[("a", 1), ("b", 2)]), &map(&[("b", 2), ("c", 3)]));
        assert_eq!(sorted(start), vec!["c"]); // c is live but not running
        assert_eq!(sorted(stop), vec!["a"]); // a is running but gone
    }

    #[test]
    fn stable_membership_is_a_noop() {
        let (stop, start) = reconcile(&map(&[("a", 1), ("b", 2)]), &map(&[("a", 1), ("b", 2)]));
        assert!(start.is_empty());
        assert!(stop.is_empty());
    }

    #[test]
    fn replaced_instance_stops_stale_and_starts_new() {
        // Same id, different Arc identity (config-overlay respawn): the stale
        // runner is stopped and a fresh one started for the new instance.
        let (stop, start) = reconcile(&map(&[("a", 1)]), &map(&[("a", 99)]));
        assert_eq!(stop, vec!["a"]);
        assert_eq!(start, vec!["a"]);
    }

    #[test]
    fn first_reconcile_starts_all_and_empty_registry_stops_all() {
        let (stop, start) = reconcile(&map(&[]), &map(&[("a", 1), ("b", 2)]));
        assert_eq!(sorted(start), vec!["a", "b"]);
        assert!(stop.is_empty());

        let (stop, start) = reconcile(&map(&[("a", 1), ("b", 2)]), &map(&[]));
        assert!(start.is_empty());
        assert_eq!(sorted(stop), vec!["a", "b"]);
    }
}
