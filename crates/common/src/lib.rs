// TEST: This comment was added by the feature impl workflow test (DW-481)
pub mod activity_log;
pub mod admin;
pub mod api_types;
pub mod backend;
pub mod backoff;
pub mod breaker;
pub mod channels;
pub mod config;
pub mod config_expand;
pub mod config_files;
pub mod config_overlay;
pub mod cost;
pub mod cost_analytics;
pub mod delivered_text;
pub mod delivery;
pub mod emoji;
pub mod engagement;
pub mod errors;
pub mod heartbeat_check;
pub mod identity;
pub mod instance;
pub mod learning_plan;
pub mod locking;
pub mod logging;
pub mod metabase_schema;
#[cfg(feature = "postgres")]
pub mod metabase_schema_postgres;
pub mod pagination;
pub mod permissions;
pub mod persistence;
pub mod playbook_state;
pub mod private_fs;
/// Backwards-compatibility re-export of the shared postgres
/// infrastructure. The actual implementation now lives in the
/// `workforce-postgres` crate to avoid a dependency cycle with
/// `workforce-pairing` (which `workforce-common` depends on for
/// `PairingConfig`). New callers should depend on `workforce-postgres`
/// directly.
pub use workforce_postgres as postgres;
pub mod oversight;
pub mod persona;
pub mod pg_text;
pub mod quarantine;
pub mod redact;
pub mod rich;
pub mod roles;
pub mod secret;
pub mod secret_lock;
pub mod secret_marker;
pub mod secret_policy;
pub mod security_audit;
pub mod security_scanner;
pub mod sentinel_config;
pub mod session;
#[cfg(feature = "postgres")]
pub mod session_migrate;
#[cfg(feature = "postgres")]
pub mod session_postgres;
pub mod settings;
pub mod shadow;
#[cfg(feature = "postgres")]
pub mod shadow_postgres;
pub mod slack_claim;
#[cfg(feature = "postgres")]
pub mod slack_claim_postgres;
pub mod stream_redact;
pub mod subscription;
pub mod telemetry;
#[cfg(feature = "postgres")]
pub mod telemetry_migrate;
#[cfg(feature = "postgres")]
pub mod telemetry_postgres;
pub mod temporal;
pub mod tenancy;
pub mod text;
pub mod timezone;
pub mod tls;
pub mod tool_input;
pub mod trust;
pub mod vault_scope;
pub mod vectorizable;
