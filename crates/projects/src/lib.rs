//! Per-agent multi-repo project store.

pub mod model;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "postgres")]
pub mod postgres_migrate;
pub mod sqlite;
pub mod store;

pub use model::{Project, ProjectDefaults, ProjectRepo, RepoSource};
pub use store::ProjectStore;
