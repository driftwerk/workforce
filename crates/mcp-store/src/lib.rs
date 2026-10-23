//! Per-agent MCP server installation persistence.
//!
//! Mirrors `workforce-projects` exactly — same trait shape, same
//! SQLite-default-with-Postgres-feature layering, same async-to-sync
//! bridge in the Postgres backend. The store is the source of truth
//! for which MCP servers a hub knows about; the in-memory
//! `McpServerRegistry` (in `workforce-hub`) is the runtime view that
//! gets hydrated from this store at startup and mutated when an
//! agent or UI client installs / uninstalls.
//!
//! Why this is its own crate rather than living in `workforce-hub`:
//! same reason `workforce-projects` is its own crate — keeps the hub
//! crate slim, allows postgres to be a non-default cargo feature, and
//! makes the persistence layer testable in isolation.

pub mod model;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "postgres")]
pub mod postgres_migrate;
pub mod sqlite;
pub mod store;

pub use model::{InstalledVia, McpServerRecord};
pub use store::McpServerStore;
