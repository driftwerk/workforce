//! Configuration and credential resolution for the direct database-access
//! tools (`database.*`).
//!
//! Connection secrets live in the vault, never in `workforce.toml`. Each
//! instance is addressed by an arbitrary name (e.g. `harbor-rails-db-prod`,
//! `core-api-db-staging`) under [`INSTANCES_PREFIX`]; a legacy single-instance
//! config under [`VAULT_PREFIX`] resolves to the name [`FLAT_INSTANCE`]. Every
//! credential read honours a per-user overlay (via [`SecretStore::get_user_override`])
//! before the shared value, so an individual analyst can point an instance at
//! their own read credential. The vault credential namespace is denied to the
//! agent's `vault.*` tools (see `hub::vault_policy::agent_can_read`), so a
//! connection string with an embedded password can never be read back into
//! context — the agent only ever names an instance.

use std::collections::BTreeMap;
use std::time::Duration;

use workforce_common::settings::SecretStore;

use super::backend::Engine;

/// Top-level vault namespace for database credentials.
pub const VAULT_PREFIX: &str = "databases/";
/// Prefix under which named instances live: `databases/instances/<name>/<field>`.
pub const INSTANCES_PREFIX: &str = "databases/instances/";
/// Instance name a legacy single-instance config (`databases/<field>`) maps to.
pub const FLAT_INSTANCE: &str = "default";
/// Vault path holding the default instance name when `instance` is omitted.
pub const DEFAULT_INSTANCE_PATH: &str = "databases/default_instance";

/// Row cap for `database.query` when no explicit `limit` is given — polite to
/// the source database. Context safety is owned by artifact staging, not this.
pub const DEFAULT_MAX_QUERY_ROWS: usize = 2_000;
/// Hard ceiling on `database.query` rows. Large pulls stage as artifacts, so
/// this bounds database load and artifact size, not context.
pub const STAGED_MAX_QUERY_ROWS: usize = 50_000;
/// Default per-query statement timeout (overridable per instance).
pub const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 30_000;

/// Non-secret descriptor of one configured database instance. The connection
/// string is resolved separately at connect time and never stored here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceConfig {
    pub name: String,
    pub engine: Engine,
    pub allow_writes: bool,
    pub max_query_rows: usize,
    pub statement_timeout: Duration,
    /// MySQL only: use TLS when connecting. Default on, so managed endpoints
    /// like Cloud SQL work out of the box; set `ssl=false` for a plaintext-only
    /// server. The Postgres family takes `sslmode` from the DSN.
    pub ssl: bool,
    /// Skip TLS certificate verification (encrypt without authenticating the
    /// peer). Default `true` because managed endpoints (Cloud SQL, RDS) present
    /// per-instance/private-CA certs not in any public trust store; set
    /// `tls_verify=true` (→ `tls_insecure=false`) for a publicly-trusted cert.
    pub tls_insecure: bool,
}

impl InstanceConfig {
    /// A short, secret-free summary for `database.list_instances`.
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "instance": self.name,
            "engine": self.engine.as_str(),
            "writable": self.allow_writes,
            "max_query_rows": self.max_query_rows,
            "statement_timeout_ms": self.statement_timeout.as_millis() as u64,
        })
    }
}

/// Read a secret honouring the per-user overlay before the shared value.
pub fn resolve_at(store: &dyn SecretStore, path: &str) -> Option<String> {
    store.get_user_override(path).or_else(|| store.get(path))
}

fn instance_field(name: &str, field: &str) -> String {
    format!("{INSTANCES_PREFIX}{name}/{field}")
}

fn flat_field(field: &str) -> String {
    format!("{VAULT_PREFIX}{field}")
}

/// Resolve the connection string for an instance (per-user overlay first).
pub fn resolve_dsn(store: &dyn SecretStore, name: &str) -> Option<String> {
    let path = if name == FLAT_INSTANCE {
        flat_field("dsn")
    } else {
        instance_field(name, "dsn")
    };
    resolve_at(store, &path)
}

fn parse_bool(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn read_config_at(
    store: &dyn SecretStore,
    name: &str,
    get: impl Fn(&str) -> String,
) -> Option<InstanceConfig> {
    let field = |f: &str| resolve_at(store, &get(f));
    let engine = field("engine")
        .and_then(|e| Engine::parse(&e))
        .or_else(|| field("dsn").and_then(|d| Engine::infer_from_dsn(&d)))?;
    let max_query_rows = field("max_query_rows")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|n| n.clamp(1, STAGED_MAX_QUERY_ROWS))
        .unwrap_or(STAGED_MAX_QUERY_ROWS);
    let statement_timeout = Duration::from_millis(
        field("statement_timeout_ms")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_STATEMENT_TIMEOUT_MS),
    );
    Some(InstanceConfig {
        name: name.to_string(),
        engine,
        allow_writes: field("allow_writes").is_some_and(|v| parse_bool(&v)),
        max_query_rows,
        statement_timeout,
        ssl: field("ssl").is_none_or(|v| parse_bool(&v)),
        tls_insecure: field("tls_verify").is_none_or(|v| !parse_bool(&v)),
    })
}

fn read_named(store: &dyn SecretStore, name: &str) -> Option<InstanceConfig> {
    read_config_at(store, name, |f| instance_field(name, f))
}

fn read_flat(store: &dyn SecretStore) -> Option<InstanceConfig> {
    read_config_at(store, FLAT_INSTANCE, flat_field)
}

/// Every configured instance, sorted by name. Reads only non-secret metadata
/// (and a DSN only to infer a missing `engine`); connection strings are
/// resolved lazily at connect time.
pub fn list_instances(store: &dyn SecretStore) -> Vec<InstanceConfig> {
    let mut out: BTreeMap<String, InstanceConfig> = BTreeMap::new();
    if let Some(cfg) = read_flat(store) {
        out.insert(cfg.name.clone(), cfg);
    }
    for path in store.list(Some(INSTANCES_PREFIX)) {
        let Some(name) = path
            .strip_prefix(INSTANCES_PREFIX)
            .and_then(|rest| rest.split('/').next())
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        if out.contains_key(name) {
            continue;
        }
        if let Some(cfg) = read_named(store, name) {
            out.insert(name.to_string(), cfg);
        }
    }
    out.into_values().collect()
}

/// The configured default instance name, if set.
pub fn default_instance(store: &dyn SecretStore) -> Option<String> {
    resolve_at(store, DEFAULT_INSTANCE_PATH).filter(|s| !s.trim().is_empty())
}

/// Clamp a caller-supplied `limit` to the effective row cap. No `limit` → the
/// polite default (unless the instance cap is smaller); an explicit `limit` is
/// honoured up to the instance cap (results stage as artifacts).
pub fn clamp_limit(requested: Option<usize>, max_rows: usize) -> usize {
    let cap = max_rows.clamp(1, STAGED_MAX_QUERY_ROWS);
    match requested {
        None | Some(0) => cap.min(DEFAULT_MAX_QUERY_ROWS),
        Some(n) => n.min(cap),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::RwLock;

    #[derive(Default)]
    struct InMem {
        shared: HashMap<String, String>,
        user: RwLock<HashMap<String, String>>,
    }
    impl InMem {
        fn with(pairs: &[(&str, &str)]) -> Self {
            Self {
                shared: pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                user: RwLock::new(HashMap::new()),
            }
        }
    }
    impl SecretStore for InMem {
        fn get(&self, path: &str) -> Option<String> {
            self.shared.get(path).cloned()
        }
        fn set(&self, _path: &str, _value: &str) -> eyre::Result<()> {
            Ok(())
        }
        fn delete(&self, _path: &str) -> eyre::Result<bool> {
            Ok(false)
        }
        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            self.shared
                .keys()
                .filter(|k| prefix.is_none_or(|p| k.starts_with(p)))
                .cloned()
                .collect()
        }
        fn get_user_override(&self, path: &str) -> Option<String> {
            self.user.read().unwrap().get(path).cloned()
        }
    }

    #[test]
    fn clamp_limit_enforces_caps() {
        assert_eq!(
            clamp_limit(None, STAGED_MAX_QUERY_ROWS),
            DEFAULT_MAX_QUERY_ROWS
        );
        assert_eq!(clamp_limit(None, 500), 500);
        assert_eq!(clamp_limit(Some(0), 500), 500);
        assert_eq!(clamp_limit(Some(100), 2000), 100);
        assert_eq!(clamp_limit(Some(10_000), 10_000), 10_000);
        assert_eq!(clamp_limit(Some(100_000), 100_000), STAGED_MAX_QUERY_ROWS);
        assert_eq!(clamp_limit(Some(1000), 200), 200);
    }

    #[test]
    fn lists_named_instances_with_engine_and_writes() {
        let store = InMem::with(&[
            ("databases/instances/pw-prod/dsn", "postgres://u@h/db"),
            ("databases/instances/pw-prod/allow_writes", "false"),
            ("databases/instances/core-api-stg/dsn", "mysql://u@h/db"),
            ("databases/instances/core-api-stg/engine", "mysql"),
            ("databases/instances/core-api-stg/allow_writes", "true"),
        ]);
        let list = list_instances(&store);
        assert_eq!(list.len(), 2);
        let pw = list.iter().find(|c| c.name == "pw-prod").unwrap();
        assert_eq!(pw.engine, Engine::Postgres);
        assert!(!pw.allow_writes);
        let km = list.iter().find(|c| c.name == "core-api-stg").unwrap();
        assert_eq!(km.engine, Engine::Mysql);
        assert!(km.allow_writes);
    }

    #[test]
    fn flat_config_resolves_to_default_instance() {
        let store = InMem::with(&[("databases/dsn", "postgresql://u@h/db")]);
        let list = list_instances(&store);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, FLAT_INSTANCE);
        assert_eq!(list[0].engine, Engine::Postgres);
        assert_eq!(
            resolve_dsn(&store, FLAT_INSTANCE).as_deref(),
            Some("postgresql://u@h/db")
        );
    }

    #[test]
    fn explicit_engine_overrides_dsn_inference_and_redshift_parses() {
        let store = InMem::with(&[
            (
                "databases/instances/warehouse/dsn",
                "postgres://u@h:5439/dw",
            ),
            ("databases/instances/warehouse/engine", "redshift"),
        ]);
        let list = list_instances(&store);
        assert_eq!(list[0].engine, Engine::Redshift);
    }

    #[test]
    fn tls_posture_defaults_insecure_and_respects_tls_verify() {
        let store = InMem::with(&[
            ("databases/instances/a/dsn", "postgres://u@h/db"),
            ("databases/instances/b/dsn", "postgres://u@h/db"),
            ("databases/instances/b/tls_verify", "true"),
        ]);
        let list = list_instances(&store);
        assert!(list.iter().find(|c| c.name == "a").unwrap().tls_insecure);
        assert!(!list.iter().find(|c| c.name == "b").unwrap().tls_insecure);
    }

    #[test]
    fn per_user_overlay_wins_for_dsn() {
        let store = InMem::with(&[("databases/instances/pw-prod/dsn", "postgres://shared@h/db")]);
        store.user.write().unwrap().insert(
            "databases/instances/pw-prod/dsn".to_string(),
            "postgres://mine@h/db".to_string(),
        );
        assert_eq!(
            resolve_dsn(&store, "pw-prod").as_deref(),
            Some("postgres://mine@h/db")
        );
    }
}
