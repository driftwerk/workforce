//! The floor's log of turns: opened once when the hub has Postgres, written
//! through one ordered writer, pruned on a timer. What it holds and how it
//! reads is `workforce_desk::floor`.

#[cfg(feature = "postgres")]
use std::sync::Arc;

use workforce_common::settings::WorkforceConfig;

#[cfg(feature = "postgres")]
pub(crate) mod agents;
#[cfg(feature = "postgres")]
mod log;
#[cfg(feature = "postgres")]
pub mod plans;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "postgres")]
pub mod signal;

#[cfg(feature = "postgres")]
pub use log::{installed, turns, Installed};

/// What the desk reports to: the installed log and plan readings.
#[cfg(feature = "postgres")]
pub fn desk_floor() -> workforce_desk::Floor {
    let installed = installed();
    workforce_desk::Floor {
        turns: installed.map(|i| Arc::clone(&i.log)),
        plans: installed.map(|i| Arc::clone(&i.plans) as Arc<dyn workforce_desk::usage::PlanSink>),
    }
}

/// For everyone the floor keeps turns or plan readings of, how many.
pub async fn holders() -> eyre::Result<std::collections::BTreeMap<String, usize>> {
    #[cfg(feature = "postgres")]
    if let Some(installed) = installed() {
        return installed.store.holders().await;
    }
    Ok(Default::default())
}

/// How many turns and plan readings the floor keeps of `person`.
pub async fn held(person: &str) -> eyre::Result<(u64, u64)> {
    #[cfg(feature = "postgres")]
    if let Some(installed) = installed() {
        return installed.store.held(person).await;
    }
    let _ = person;
    Ok((0, 0))
}

/// Makes `from`'s turns and plan readings `into`'s; how many moved.
pub async fn reassign(from: &str, into: &str) -> eyre::Result<u64> {
    #[cfg(feature = "postgres")]
    if let Some(installed) = installed() {
        return installed.store.reassign(from, into).await;
    }
    let _ = (from, into);
    Ok(0)
}

/// Removes `person`'s turns and plan readings; how many went.
pub async fn forget(person: &str) -> eyre::Result<u64> {
    #[cfg(feature = "postgres")]
    if let Some(installed) = installed() {
        return installed.store.forget(person).await;
    }
    let _ = person;
    Ok(0)
}

/// Opens the log once and keeps it pruned; nothing without Postgres.
pub async fn install(config: &WorkforceConfig) {
    #[cfg(feature = "postgres")]
    log::install(config).await;
    #[cfg(not(feature = "postgres"))]
    let _ = config;
}
