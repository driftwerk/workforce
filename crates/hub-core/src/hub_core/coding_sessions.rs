//! How the service reloads a coding session once a login lands.

use std::sync::Arc;

/// Idempotent re-register hook for the `coding.session` tool. Called after a
/// successful token write so the tool appears in the same turn — no restart.
pub type ReloadCodingSession = Arc<dyn Fn() + Send + Sync>;
