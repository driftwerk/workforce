//! The floor's log of turns: opened once when the hub has Postgres, written
//! through one ordered writer, pruned on a timer. What it holds and how it
//! reads is `workforce_desk::floor`.

use workforce_common::settings::WorkforceConfig;

#[cfg(feature = "postgres")]
mod log;
#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "postgres")]
pub use log::turns;

/// Opens the log once and keeps it pruned; nothing without Postgres.
pub async fn install(config: &WorkforceConfig) {
    #[cfg(feature = "postgres")]
    log::install(config).await;
    #[cfg(not(feature = "postgres"))]
    let _ = config;
}
