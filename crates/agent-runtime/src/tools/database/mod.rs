//! Direct database-access tools (`database.*`) — an essential built-in family
//! (registered in the core toolset, not a loadable skill) for querying and
//! managing customer databases directly: Postgres, Redshift, Cloud SQL, AlloyDB
//! (Postgres family) and MySQL / Cloud SQL-MySQL.
//!
//!
//! Safety, primary → last: a least-privilege (`GRANT SELECT`) database role is
//! the real boundary; every read additionally runs in a `READ ONLY` transaction
//! with a `statement_timeout`; statement classification is a usability guard
//! only. Connection strings live in the vault (namespace `databases/`, denied to
//! the agent's `vault.*` tools) and never enter context — the agent names an
//! instance, never a DSN. Writes (`database.execute`) require a writable
//! instance, the SystemAdmin role, and per-call approval, and are audited.

pub mod backend;
pub mod config;
mod export;
mod tools;

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use eyre::{eyre, Result};
use tokio::sync::Mutex;
use workforce_common::settings::SecretStore;

use crate::llm::environment::{resolve_instance, InstanceSource, SkillVariants};
use crate::runner::current_active_environment;
use crate::tools::access::{ToolAccess, ToolAccessSnapshot};
use crate::tools::registry::ToolRegistry;
use crate::tools::Tool;

use backend::{mysql::MyBackend, postgres::PgBackend, Backend, Engine};
use config::InstanceConfig;

/// SystemAdmin gate, structurally identical to `hub::authz::IsAdminFn`.
pub type AdminCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// Every `database.*` tool name, in registration order.
pub const TOOL_NAMES: &[&str] = &[
    "database.list_instances",
    "database.schemas",
    "database.tables",
    "database.columns",
    "database.query",
    "database.explain",
    "database.export",
    "database.execute",
];

/// The one tool that answers without an instance: it is how an agent finds out
/// that none is configured and what an operator does about it.
const LIST_INSTANCES: &str = "database.list_instances";

/// What a `database.*` call is told while the caller has no instance. It names
/// the way forward because the usual caller wants a database this install
/// reaches through another tool, and setup steps alone send it nowhere. It
/// says nothing of the install as a whole: an instance may be configured for
/// one user only, and another user's call is answered with this same text.
const NO_INSTANCE: &str = "No database instance is available to you here, so the database.* \
    tools cannot run — do not retry them. They reach databases an operator has connected \
    directly; `database.list_instances` explains that setup, which the agent cannot do itself. A \
    database reached through another connected tool is read with that tool instead — where the \
    Metabase tools are offered, `metabase.table_fields` for a table's columns and \
    `metabase.dataset_query_native` for SQL.";

/// Hides the tools that need an instance from a caller who has none, so an
/// install without direct database connections does not offer them at all.
/// Read on every snapshot through the same store the tools resolve from —
/// its per-user levels included, so an instance one user configured shows the
/// tools to that user alone — and adding one in the vault brings them back
/// without a restart. The store answers from memory, so a round pays no I/O.
pub struct DatabaseToolAccess {
    store: Arc<dyn SecretStore>,
}

impl DatabaseToolAccess {
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self { store }
    }
}

impl ToolAccess for DatabaseToolAccess {
    fn snapshot(&self) -> ToolAccessSnapshot {
        match config::list_instances(self.store.as_ref()).is_empty() {
            true => ToolAccessSnapshot::new(
                TOOL_NAMES
                    .iter()
                    .filter(|name| **name != LIST_INSTANCES)
                    .map(|name| (name.to_string(), NO_INSTANCE.to_string()))
                    .collect(),
            ),
            false => ToolAccessSnapshot::default(),
        }
    }
}

struct Cached {
    fingerprint: u64,
    backend: Arc<dyn Backend>,
}

/// Shared state for the database tools: the credential store plus a lazily
/// populated, fingerprint-keyed cache of connection pools (one per instance,
/// rebuilt when its configuration changes).
pub struct DatabaseRuntime {
    store: Arc<dyn SecretStore>,
    pools: Mutex<HashMap<String, Cached>>,
}

impl DatabaseRuntime {
    pub fn new(store: Arc<dyn SecretStore>) -> Arc<Self> {
        Arc::new(Self {
            store,
            pools: Mutex::new(HashMap::new()),
        })
    }

    /// Configured instances, re-read each call so vault edits take effect
    /// without a restart.
    pub fn instances(&self) -> Vec<InstanceConfig> {
        config::list_instances(self.store.as_ref())
    }

    fn default_name(&self, list: &[InstanceConfig]) -> String {
        config::default_instance(self.store.as_ref())
            .filter(|d| list.iter().any(|c| &c.name == d))
            .or_else(|| (list.len() == 1).then(|| list[0].name.clone()))
            .or_else(|| {
                list.iter()
                    .find(|c| c.name == config::FLAT_INSTANCE)
                    .map(|c| c.name.clone())
            })
            .unwrap_or_default()
    }

    /// Resolve the target instance (honouring an active environment's pin) and
    /// return its config plus a connected backend.
    pub async fn resolve(
        &self,
        requested: Option<&str>,
    ) -> Result<(InstanceConfig, Arc<dyn Backend>)> {
        let list = self.instances();
        if list.is_empty() {
            return Err(eyre!(NO_INSTANCE));
        }
        let default = self.default_name(&list);
        if requested.is_none() && default.is_empty() && current_active_environment().is_none() {
            let names = list
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(eyre!(
                "Multiple database instances configured ({names}). Pass `instance`, or an \
                 operator sets a default from the host with \
                 `workforce vault set databases/default_instance '<name>'`."
            ));
        }
        let by_name: HashMap<String, InstanceConfig> =
            list.into_iter().map(|c| (c.name.clone(), c)).collect();
        let name = resolve_instance("database", requested, &default, &by_name)
            .map_err(|d| eyre!("{}", d.message))?;
        let cfg = by_name.get(&name).cloned().ok_or_else(|| {
            let mut avail: Vec<&str> = by_name.keys().map(String::as_str).collect();
            avail.sort_unstable();
            eyre!(
                "Unknown database instance '{name}'. Available: {}",
                avail.join(", ")
            )
        })?;
        let backend = self.backend_for(&cfg).await?;
        Ok((cfg, backend))
    }

    async fn backend_for(&self, cfg: &InstanceConfig) -> Result<Arc<dyn Backend>> {
        let dsn = config::resolve_dsn(self.store.as_ref(), &cfg.name).ok_or_else(|| {
            eyre!(
                "database instance '{}' has no connection string. An operator sets it from the \
                 host with `workforce vault set databases/instances/{}/dsn '<connection string>'`.",
                cfg.name,
                cfg.name
            )
        })?;
        let fingerprint = fingerprint(cfg, &dsn);
        let mut pools = self.pools.lock().await;
        if let Some(cached) = pools.get(&cfg.name) {
            if cached.fingerprint == fingerprint {
                return Ok(Arc::clone(&cached.backend));
            }
        }
        let backend = build_backend(cfg, &dsn)?;
        pools.insert(
            cfg.name.clone(),
            Cached {
                fingerprint,
                backend: Arc::clone(&backend),
            },
        );
        Ok(backend)
    }
}

fn fingerprint(cfg: &InstanceConfig, dsn: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    cfg.engine.as_str().hash(&mut hasher);
    dsn.hash(&mut hasher);
    cfg.allow_writes.hash(&mut hasher);
    cfg.ssl.hash(&mut hasher);
    cfg.tls_insecure.hash(&mut hasher);
    cfg.statement_timeout.hash(&mut hasher);
    cfg.max_query_rows.hash(&mut hasher);
    hasher.finish()
}

fn build_backend(cfg: &InstanceConfig, dsn: &str) -> Result<Arc<dyn Backend>> {
    Ok(match cfg.engine {
        Engine::Postgres | Engine::Redshift => Arc::new(PgBackend::new(
            dsn,
            cfg.engine,
            cfg.allow_writes,
            cfg.tls_insecure,
        )?),
        Engine::Mysql => Arc::new(MyBackend::new(
            dsn,
            cfg.allow_writes,
            cfg.ssl,
            cfg.tls_insecure,
        )?),
    })
}

impl InstanceSource for DatabaseRuntime {
    fn variants(&self) -> Option<SkillVariants> {
        let list = self.instances();
        SkillVariants::of(
            list.iter().map(|c| c.name.clone()),
            &self.default_name(&list),
        )
    }
}

/// Register the `database.*` tool set and its instances on `registry`. `store`
/// is the layered user vault; `is_admin` is the shared SystemAdmin gate (gates
/// `database.execute`).
pub fn register_database_tools(
    registry: &ToolRegistry,
    store: Arc<dyn SecretStore>,
    is_admin: AdminCheck,
) {
    let rt = DatabaseRuntime::new(store);
    registry.set_instance_source("database", Arc::clone(&rt) as Arc<dyn InstanceSource>);
    database_tools(rt, is_admin)
        .into_iter()
        .for_each(|tool| registry.register(tool));
}

fn database_tools(rt: Arc<DatabaseRuntime>, is_admin: AdminCheck) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(tools::ListInstancesTool::new(Arc::clone(&rt))),
        Arc::new(tools::SchemasTool::new(Arc::clone(&rt))),
        Arc::new(tools::TablesTool::new(Arc::clone(&rt))),
        Arc::new(tools::ColumnsTool::new(Arc::clone(&rt))),
        Arc::new(tools::QueryTool::new(Arc::clone(&rt))),
        Arc::new(tools::ExplainTool::new(Arc::clone(&rt))),
        Arc::new(tools::ExportTool::new(Arc::clone(&rt))),
        Arc::new(tools::ExecuteTool::new(Arc::clone(&rt), is_admin)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Vault(HashMap<String, String>);

    impl Vault {
        fn with(pairs: &[(&str, &str)]) -> Arc<dyn SecretStore> {
            Arc::new(Self(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ))
        }
    }

    impl SecretStore for Vault {
        fn get(&self, path: &str) -> Option<String> {
            self.0.get(path).cloned()
        }
        fn set(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn delete(&self, _: &str) -> Result<bool> {
            Ok(false)
        }
        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            self.0
                .keys()
                .filter(|k| prefix.is_none_or(|p| k.starts_with(p)))
                .cloned()
                .collect()
        }
    }

    fn blocked(store: Arc<dyn SecretStore>) -> ToolAccessSnapshot {
        DatabaseToolAccess::new(store).snapshot()
    }

    #[test]
    fn database_tools_that_need_an_instance_are_hidden_while_none_is_configured() {
        let snapshot = blocked(Vault::with(&[]));
        let hidden: Vec<&str> = TOOL_NAMES
            .iter()
            .copied()
            .filter(|name| snapshot.is_blocked(name))
            .collect();
        assert_eq!(hidden, &TOOL_NAMES[1..]);
    }

    #[test]
    fn list_instances_stays_offered_without_an_instance() {
        assert!(!blocked(Vault::with(&[])).is_blocked(LIST_INSTANCES));
    }

    #[test]
    fn configuring_an_instance_brings_the_database_tools_back() {
        let store = Vault::with(&[(
            "databases/instances/main/dsn",
            "postgres://ro@db.internal/app",
        )]);
        assert!(blocked(store).is_empty());
    }

    #[test]
    fn a_hidden_database_call_is_told_which_tools_read_the_data_instead() {
        let snapshot = blocked(Vault::with(&[]));
        let reason = snapshot.block_reason("database.columns").unwrap();
        assert!(reason.contains("do not retry"));
        assert!(reason.contains("metabase.table_fields"));
        assert!(reason.contains("database.list_instances"));
    }

    #[tokio::test]
    async fn a_database_call_without_an_instance_fails_with_the_same_guidance() {
        let rt = DatabaseRuntime::new(Vault::with(&[]));
        let error = rt.resolve(None).await.err().unwrap().to_string();
        assert_eq!(error, NO_INSTANCE);
    }

    #[test]
    fn every_registered_database_tool_is_named_in_tool_names() {
        let registry = ToolRegistry::new();
        register_database_tools(&registry, Vault::with(&[]), Arc::new(|| false));
        let mut expected = TOOL_NAMES.to_vec();
        expected.sort_unstable();
        assert_eq!(registry.names(), expected);
    }
}
