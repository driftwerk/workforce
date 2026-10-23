//! Optional Postgres backend infrastructure shared by every workforce
//! store that opts into Postgres.
//!
//! [`PostgresConfig`] is always available so it can sit in
//! `WorkforceConfig` regardless of cargo features. The runtime types
//! ([`PostgresPool`], [`PostgresError`], the migration runner) are
//! gated behind the `postgres` cargo feature so default builds stay
//! slim. See `docs/POSTGRES.md` for the migration roadmap.
//!
//! ## Per-store pool architecture
//!
//! Each store builds its own [`PostgresPool`] from this crate's
//! `PostgresConfig` using `PostgresPool::from_config_with_session_init`
//! so it can pin `search_path` to its own schema via a deadpool
//! `post_create` hook. Pools are not literally shared between stores —
//! sharing one pool across stores with different schemas would mean
//! fighting Postgres' per-session `search_path`.
//!
//! ## Why this is its own crate (not a module of `workforce-common`)
//!
//! `workforce-common` already depends on `workforce-pairing` for the
//! `PairingConfig` type that lives in `WorkforceConfig`. Per-store
//! crates like `workforce-pairing` and `workforce-identity` need
//! Postgres support, so they need to depend on the postgres
//! infrastructure — but they can't depend on `workforce-common`
//! without creating a cycle. Hence: a separate, dependency-free crate.

mod config;
pub use config::{default_backend_connect_timeout_secs, PostgresConfig, SslMode};

#[cfg(feature = "postgres")]
mod error;
#[cfg(feature = "postgres")]
mod migrations;
#[cfg(feature = "postgres")]
mod pool;

#[cfg(feature = "postgres")]
pub use error::PostgresError;
#[cfg(feature = "postgres")]
pub use migrations::{MigrationRun, MigrationRunner, MigrationSet};
#[cfg(feature = "postgres")]
pub use pool::PostgresPool;

/// Session statement that makes every transaction on a connection
/// read-only: for offline readers that must not change a live install.
pub const READ_ONLY_SESSION: &str = "SET default_transaction_read_only = on";

#[cfg(feature = "test-support")]
pub mod test_support;
