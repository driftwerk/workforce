//! Shared concurrency budget for Claude Code subprocesses.
//!
//! Every Claude Code subprocess the hub spawns on an agent's behalf — the
//! `advisor` (single review + each fan-out lens + the synth) and each
//! `coding.fanout` coder — is heavy (a `claude` process and its MCP session)
//! and they multiply under fan-out. They all draw from ONE budget here
//! so the two surfaces can't independently exceed the host's memory ceiling.
//!
//! Two nested gates are acquired in a uniform order (no lock-ordering
//! deadlock): a per-agent gate (`advisor.fanout.max_concurrent`) for fairness
//! so one agent can't monopolise the host across advisor and coding work, then
//! the hub-wide gate (`advisor.fanout.hub_max_concurrent_cc`) for host safety —
//! the aggregate ceiling across every agent and every CC surface. The hub
//! permit is the terminal acquire and is only ever held by a live subprocess,
//! so it always drains. Over-subscription (the per-agent caps summing past the
//! hub cap) is intended: the hub gate is the hard ceiling regardless, and the
//! per-agent gate only bounds any single agent's share. The caps live under
//! `[advisor.fanout]` for historical reasons but govern all CC subprocesses.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::serve::AgentService;

/// Per-agent limiters, keyed by agent id, caching the cap each was sized for so
/// a hot-reloaded cap change rebuilds the semaphore (in-flight permits drain
/// against the old one — a brief, bounded overshoot).
static CC_AGENT_SEMS: OnceLock<Mutex<HashMap<String, (u32, Arc<Semaphore>)>>> = OnceLock::new();
/// Hub-wide ceiling. Sized once from the global config (restart to change),
/// matching the global-config-is-restart-only model.
static CC_HUB_SEM: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// Resolve the per-agent limiter for `cap` permits (`None` = unbounded).
/// Rebuilds the semaphore when the cached cap differs from `cap` (hot-reload).
pub fn agent_cc_sem(agent_id: &str, cap: u32) -> Option<Arc<Semaphore>> {
    if cap == 0 {
        return None;
    }
    let mut map = CC_AGENT_SEMS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock();
    if let Some((cached, sem)) = map.get(agent_id) {
        if *cached == cap {
            return Some(Arc::clone(sem));
        }
    }
    let sem = Arc::new(Semaphore::new(cap as usize));
    map.insert(agent_id.to_string(), (cap, Arc::clone(&sem)));
    Some(sem)
}

/// Resolve the hub-wide limiter for `cap` permits (`None` = unbounded). Sized
/// once; later cap changes need a restart (global config).
pub fn hub_cc_sem(cap: u32) -> Option<Arc<Semaphore>> {
    if cap == 0 {
        return None;
    }
    Some(Arc::clone(
        CC_HUB_SEM.get_or_init(|| Arc::new(Semaphore::new(cap as usize))),
    ))
}

/// RAII holder for the acquired permits — released when dropped (the explicit
/// post-subprocess drop, AND future-drop when a caller times out / is
/// cancelled), so a slot is never held longer than its subprocess.
pub struct CcPermits {
    _agent: Option<OwnedSemaphorePermit>,
    _hub: Option<OwnedSemaphorePermit>,
}

/// Acquire a permit from an optional limiter, abortable by `cancel`.
/// `Ok(None)` = no limiter (unbounded); `Ok(Some)` = held permit; `Err(())` =
/// cancelled while waiting. A closed semaphore (never happens here) maps to
/// `Ok(None)` — fail-open rather than block.
pub async fn acquire_cc_permit(
    sem: Option<Arc<Semaphore>>,
    cancel: Option<&CancellationToken>,
) -> Result<Option<OwnedSemaphorePermit>, ()> {
    let Some(sem) = sem else {
        return Ok(None);
    };
    match cancel {
        Some(tok) => tokio::select! {
            biased;
            _ = tok.cancelled() => Err(()),
            permit = sem.acquire_owned() => Ok(permit.ok()),
        },
        None => Ok(sem.acquire_owned().await.ok()),
    }
}

/// Acquire one per-agent + one hub-wide permit, agent-gate first. Returns `None`
/// if `cancel` fires while waiting, so the caller aborts rather than queueing
/// behind a saturated limiter. Per-agent cap is read live (hot-reload); the hub
/// cap is the global value (restart-only).
pub async fn acquire(
    service: &Arc<AgentService>,
    cancel: Option<&CancellationToken>,
) -> Option<CcPermits> {
    let agent_cap = service.effective_advisor().fanout.max_concurrent;
    let hub_cap = service.config.advisor.fanout.hub_max_concurrent_cc;
    let agent = acquire_cc_permit(agent_cc_sem(service.agent_id.as_str(), agent_cap), cancel)
        .await
        .ok()?;
    let hub = acquire_cc_permit(hub_cc_sem(hub_cap), cancel).await.ok()?;
    Some(CcPermits {
        _agent: agent,
        _hub: hub,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_cc_sem_zero_cap_is_unbounded() {
        assert!(
            agent_cc_sem("test-cc-unbounded", 0).is_none(),
            "cap 0 must mean no limiter (unbounded)"
        );
        assert!(hub_cc_sem(0).is_none());
    }

    #[test]
    fn agent_cc_sem_sizes_and_caches_per_agent() {
        // Unique id so this doesn't race other tests' global map entries.
        let id = "test-cc-cache-agent";
        let a = agent_cc_sem(id, 3).expect("sized");
        assert_eq!(a.available_permits(), 3);
        // Same cap → same semaphore instance (cached).
        let b = agent_cc_sem(id, 3).expect("sized");
        assert!(
            Arc::ptr_eq(&a, &b),
            "same cap must reuse the cached semaphore"
        );
        // Changed cap (hot-reload) → fresh semaphore with the new size.
        let c = agent_cc_sem(id, 5).expect("sized");
        assert!(
            !Arc::ptr_eq(&a, &c),
            "a changed cap must rebuild the semaphore"
        );
        assert_eq!(c.available_permits(), 5);
    }

    #[tokio::test]
    async fn agent_cc_sem_permit_bounds_and_releases() {
        let id = "test-cc-permit-bound";
        let sem = agent_cc_sem(id, 2).expect("sized");
        let p1 = sem.clone().acquire_owned().await.unwrap();
        let _p2 = sem.clone().acquire_owned().await.unwrap();
        assert_eq!(sem.available_permits(), 0, "both permits held");
        drop(p1);
        assert_eq!(sem.available_permits(), 1, "dropping a permit frees a slot");
    }

    #[tokio::test]
    async fn acquire_cc_permit_no_limiter_is_ok_none() {
        assert!(matches!(acquire_cc_permit(None, None).await, Ok(None)));
    }

    #[tokio::test]
    async fn acquire_cc_permit_aborts_when_cancelled_under_saturation() {
        let sem = Arc::new(Semaphore::new(1));
        let _held = sem.clone().acquire_owned().await.unwrap(); // saturate
        let token = CancellationToken::new();
        token.cancel();
        assert!(
            acquire_cc_permit(Some(sem), Some(&token)).await.is_err(),
            "a cancelled token must abort the permit wait"
        );
    }
}
