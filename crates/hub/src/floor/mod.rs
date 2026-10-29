//! The floor's log of turns: opened once when the hub has Postgres, written
//! through one ordered writer, pruned on a timer. What it holds and how it
//! reads is `workforce_desk::floor`.

use workforce_common::settings::WorkforceConfig;

#[cfg(feature = "postgres")]
pub(crate) mod agents;
#[cfg(feature = "postgres")]
mod log;
#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "postgres")]
pub use log::turns;

/// What the desk reports to: the installed log.
#[cfg(feature = "postgres")]
pub fn desk_floor() -> workforce_desk::Floor {
    workforce_desk::Floor {
        turns: turns().cloned(),
    }
}

/// Opens the log once and keeps it pruned; nothing without Postgres.
pub async fn install(config: &WorkforceConfig) {
    #[cfg(feature = "postgres")]
    log::install(config).await;
    #[cfg(not(feature = "postgres"))]
    let _ = config;
}
