//! What a person's Desk shares with this hub's agents of their work (Sees,
//! `designs/agent-sees.md` in workforce-desk), on the agent's hub.
//!
//! Desk decides what an agent knows, on the person's own computer, and
//! pushes only that here. This hub never asks for anything: it has no route
//! that names a session, so an agent can't be given work outside what the
//! person chose for it, whatever this hub does.
//!
//! - **The token** is a `desk_knows` kind (`wfk_`) in the remote MCP token
//!   table, bound to the person, the minting device and one agent, with the
//!   engine's key registered for a later signature (Sees K8). It reaches the
//!   push, forget and status routes here and nothing else: the remote MCP
//!   routes take only `wfr_`, and nothing else knows this kind.
//! - **Mint and revoke** (`/api/me/desk-knows-tokens`) are device-only,
//!   through the paired Desk's own token, behind `[desk_knows] enabled` and
//!   the agent's own `[desk_knows] accept` (both on by default). Unpairing
//!   the device revokes its tokens.
//! - **Push, forget, status** (`/api/desk-knows/*`) take the token alone.
//!   What is kept is the person's, for that agent, from that Desk
//!   ([`store::Owner`]), and only that person's own turns with the agent
//!   read it (Sees K4).
//! - **Kept** in its own schema ([`store`]), pruned by `retain_days` and
//!   `max_mb`, and erased with the person.

pub mod routes;
pub mod store;
pub mod sync;
pub mod tools;

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tracing::warn;
use workforce_common::settings::{DeskKnowsConfig, WorkforceConfig};

use store::KnowsStore;

/// How often a running hub prunes what it keeps.
const PRUNE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

static STORE: std::sync::OnceLock<Arc<dyn KnowsStore>> = std::sync::OnceLock::new();

/// The store, once [`install`] has opened it.
pub fn installed() -> Option<Arc<dyn KnowsStore>> {
    STORE.get().cloned()
}

/// Opens the store once (Postgres only; without it nothing is kept and the
/// routes say so), and keeps it pruned.
pub async fn install(config: &WorkforceConfig) {
    if STORE.get().is_some() {
        return;
    }
    let Some(store) = open(config).await else {
        return;
    };
    if STORE.set(Arc::clone(&store)).is_ok() {
        tokio::spawn(prune_forever(store, config.desk_knows.clone()));
    }
}

async fn open(config: &WorkforceConfig) -> Option<Arc<dyn KnowsStore>> {
    #[cfg(feature = "postgres")]
    if let Some(pg) = config.postgres.as_ref().filter(|p| p.enabled) {
        return match store::postgres::PostgresKnows::new(pg).await {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                warn!(error = %format!("{e:#}"), "desk knows store unavailable; Desk can't share work with agents here");
                None
            }
        };
    }
    let _ = config;
    None
}

async fn prune_forever(store: Arc<dyn KnowsStore>, config: DeskKnowsConfig) {
    let mut tick = tokio::time::interval(PRUNE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let before = Utc::now() - chrono::Duration::days(i64::from(config.retain()));
        match store.prune(before, config.max_bytes()).await {
            Ok(0) => {}
            Ok(removed) => tracing::info!(removed, "pruned what Desk shared with agents"),
            Err(e) => warn!(error = %format!("{e:#}"), "pruning what Desk shared failed"),
        }
    }
}

/// Everything any Desk shared for `person`, gone: for erasing an identity.
pub async fn forget_person(person: uuid::Uuid) -> eyre::Result<u64> {
    match installed() {
        Some(store) => store.forget_person(person).await,
        None => Ok(0),
    }
}
