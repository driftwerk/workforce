use std::path::PathBuf;
use std::sync::Arc;

use workforce_common::admin::AuditLog;
use workforce_common::cost::CostTracker;
use workforce_common::locking::LockManager;
#[cfg(feature = "postgres")]
use workforce_common::postgres::PostgresPool;
use workforce_common::settings::WorkforceConfig;
use workforce_common::telemetry::TelemetryStore;
use workforce_distributed::node::NodeRegistry;
use workforce_memory::in_memory::InMemoryStore;
use workforce_memory::sqlite::{SqliteMemoryConfig, SqliteMemoryStore};
use workforce_memory::store::MemoryStore;
use workforce_orchestrator::orchestrator::Orchestrator;
use workforce_policy::engine::PolicyEngine;
use workforce_skills::plugin::PluginRegistry;
use workforce_skills::registry::SkillRegistry;

/// Shared application state for the hub.
#[allow(dead_code)]
pub struct HubState {
    pub config: WorkforceConfig,
    pub orchestrator: Arc<Orchestrator>,
    pub policy_engine: Arc<PolicyEngine>,
    pub memory: Arc<dyn MemoryStore>,
    pub cost_tracker: Arc<CostTracker>,
    pub audit_log: Arc<AuditLog>,
    pub node_registry: Arc<tokio::sync::RwLock<NodeRegistry>>,
    pub skill_registry: Arc<tokio::sync::RwLock<SkillRegistry>>,
    pub plugin_registry: Arc<tokio::sync::RwLock<PluginRegistry>>,
    pub lock_manager: Arc<LockManager>,
    pub telemetry_store: Arc<dyn TelemetryStore>,
    pub nats: Option<async_nats::Client>,
    pub jetstream: Option<async_nats::jetstream::Context>,
    pub scheduler: Option<Arc<crate::scheduler::Scheduler>>,
    pub workflow_runtime: Option<Arc<dyn crate::hosts::WorkflowHost>>,
    pub playbook_runtime: Option<Arc<dyn crate::hosts::PlaybookHost>>,
    /// Path to workforce.toml for config persistence (set after construction).
    pub config_path: Option<PathBuf>,
    /// Auth token for admin API write endpoints (shared with gateway).
    pub auth_token: Option<String>,
    /// Default agent's job runner, backing the `/api/subagents` admin endpoints
    /// (sub-agents are `Agent` jobs).
    pub agent_jobs: Option<Arc<dyn crate::hosts::JobHost>>,
    /// Event source manager for event source lifecycle management (DW-428).
    pub event_source_manager: Option<Arc<crate::event_manager::EventSourceManager>>,
    /// Shared knowledge service for knowledge index management (DW-428).
    pub knowledge_service: Option<Arc<crate::knowledge::KnowledgeService>>,
    /// Installed post-construction by the CLI when both stores are in
    /// scope. `OnceLock` keeps the field mutable across `Arc<HubState>`
    /// clones without dragging a runtime lock onto every read.
    pub identity_store_cell: std::sync::OnceLock<Arc<dyn workforce_identity::store::IdentityStore>>,
    pub agent_registry_cell: std::sync::OnceLock<Arc<crate::agent_registry::AgentRegistry>>,
    /// Permission writer for runtime Always-* persistence. Late-bound
    /// (see [`crate::serve::AgentService::set_permission_writer`]).
    pub permission_writer_cell:
        std::sync::OnceLock<Arc<dyn workforce_agent::runner::PermissionWriter>>,
    /// Cascade gate consulted by the worker-process runner. Late-bound
    /// from `cli::main`; `None` keeps the worker on the PolicyEngine
    /// decision verbatim.
    pub permission_gate_cell: std::sync::OnceLock<Arc<dyn workforce_agent::runner::PermissionGate>>,
    /// Constitutional admin gate consulted by the worker-process runner
    /// *before* the cascade and any prompt. Late-bound from `cli::main`;
    /// wired **always** when an identity store exists (not gated on
    /// `[permissions] enabled`).
    pub capability_gate_cell: std::sync::OnceLock<Arc<dyn workforce_agent::runner::CapabilityGate>>,
}

impl HubState {
    pub fn identity_store(&self) -> Option<&Arc<dyn workforce_identity::store::IdentityStore>> {
        self.identity_store_cell.get()
    }

    pub fn agent_registry(&self) -> Option<&Arc<crate::agent_registry::AgentRegistry>> {
        self.agent_registry_cell.get()
    }

    pub fn set_identity_store(&self, store: Arc<dyn workforce_identity::store::IdentityStore>) {
        let _ = self.identity_store_cell.set(store);
    }

    pub fn set_agent_registry(&self, registry: Arc<crate::agent_registry::AgentRegistry>) {
        let _ = self.agent_registry_cell.set(registry);
    }

    pub fn permission_writer(&self) -> Option<&Arc<dyn workforce_agent::runner::PermissionWriter>> {
        self.permission_writer_cell.get()
    }

    pub fn set_permission_writer(
        &self,
        writer: Arc<dyn workforce_agent::runner::PermissionWriter>,
    ) {
        let _ = self.permission_writer_cell.set(writer);
    }

    pub fn permission_gate(&self) -> Option<&Arc<dyn workforce_agent::runner::PermissionGate>> {
        self.permission_gate_cell.get()
    }

    pub fn set_permission_gate(&self, gate: Arc<dyn workforce_agent::runner::PermissionGate>) {
        let _ = self.permission_gate_cell.set(gate);
    }

    pub fn capability_gate(&self) -> Option<&Arc<dyn workforce_agent::runner::CapabilityGate>> {
        self.capability_gate_cell.get()
    }

    pub fn set_capability_gate(&self, gate: Arc<dyn workforce_agent::runner::CapabilityGate>) {
        let _ = self.capability_gate_cell.set(gate);
    }

    /// Hub state on `config` for tests: in-memory stores, an in-memory
    /// telemetry store, and nothing connected.
    #[doc(hidden)]
    pub fn for_tests(config: WorkforceConfig) -> Self {
        let telemetry_store: Arc<dyn TelemetryStore> = Arc::new(
            workforce_common::persistence::SqliteStore::in_memory().expect("in-memory sqlite"),
        );
        Self {
            config,
            orchestrator: Orchestrator::new(),
            policy_engine: Arc::new(PolicyEngine::new(workforce_policy::rules::PolicyConfig {
                rules: vec![],
            })),
            memory: Arc::new(InMemoryStore::new()),
            cost_tracker: Arc::new(CostTracker::with_store(Arc::clone(&telemetry_store))),
            audit_log: Arc::new(AuditLog::new(1_000)),
            node_registry: Default::default(),
            skill_registry: Arc::new(tokio::sync::RwLock::new(SkillRegistry::new())),
            plugin_registry: Arc::new(tokio::sync::RwLock::new(PluginRegistry::new())),
            lock_manager: Arc::new(LockManager::new()),
            telemetry_store,
            nats: None,
            jetstream: None,
            scheduler: None,
            workflow_runtime: None,
            playbook_runtime: None,
            config_path: None,
            auth_token: None,
            agent_jobs: None,
            event_source_manager: None,
            knowledge_service: None,
            identity_store_cell: std::sync::OnceLock::new(),
            agent_registry_cell: std::sync::OnceLock::new(),
            permission_writer_cell: std::sync::OnceLock::new(),
            permission_gate_cell: std::sync::OnceLock::new(),
            capability_gate_cell: std::sync::OnceLock::new(),
        }
    }
}

impl HubState {
    pub async fn from_config(config: WorkforceConfig) -> eyre::Result<Arc<Self>> {
        tracing::info!("Initialising hub state...");

        // Optional shared Postgres pool, constructed when `[postgres] enabled
        // = true`. The connection is retried for `connect_timeout_secs` so a
        // cold cluster-wide restart (Postgres still coming up) doesn't lose the
        // race. On exhaustion the pool stays `None` and any store *configured*
        // for Postgres fails closed at its own resolver, aborting startup —
        // never a silent SQLite downgrade.
        #[cfg(feature = "postgres")]
        let postgres_pool: Option<PostgresPool> = match config
            .postgres
            .as_ref()
            .filter(|p| p.enabled)
        {
            Some(pg_cfg) => {
                let budget = std::time::Duration::from_secs(pg_cfg.connect_timeout_secs);
                let connect = workforce_common::backend::connect_with_retry(
                    "postgres",
                    budget,
                    std::time::Duration::from_secs(5),
                    || async {
                        let pool = PostgresPool::from_config(pg_cfg)?;
                        pool.ping().await?;
                        Ok::<_, eyre::Report>(pool)
                    },
                )
                .await;
                match connect {
                    Ok(pool) => {
                        tracing::info!("Postgres pool initialised and reachable");
                        Some(pool)
                    }
                    Err(e) => {
                        tracing::warn!(error = ?e, "Postgres enabled but unreachable after the connect budget; stores configured for Postgres will fail closed");
                        None
                    }
                }
            }
            None => None,
        };

        // Policy engine — load from file or built-in default
        let policy_engine = Arc::new(crate::serve::load_policy(&config)?);

        let memory = resolve_memory_store(&config).await?;

        // Connect to NATS and initialise JetStream. Routed through
        // `workforce_nats::client::connect` so auth (`credentials`, `token`)
        // and TLS settings from `[nats]` are applied; a direct
        // `async_nats::connect(url)` here silently ignored them.
        let nats_config = workforce_nats::client::NatsConfig {
            url: config.nats.url.clone(),
            credentials_file: config
                .nats
                .credentials
                .as_ref()
                .map(std::path::PathBuf::from),
            token: config.nats.token.clone(),
            require_tls: config.nats.tls,
            ..Default::default()
        };
        let (nats, jetstream) = match workforce_nats::client::connect(&nats_config).await {
            Ok((client, js)) => {
                // Bounded: this runs on the boot path before the gateway
                // binds, and a wedged JetStream must degrade, not hang boot.
                let stream_mgr = workforce_nats::streams::StreamManager::new(
                    js.clone(),
                    config.nats.stream_replicas,
                );
                match tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    stream_mgr.ensure_all_streams(),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!("Failed to initialise JetStream streams: {e}"),
                    Err(_) => tracing::warn!("JetStream stream init timed out after 15s"),
                }
                (Some(client), Some(js))
            }
            Err(e) => {
                tracing::warn!("NATS unavailable ({}), running in standalone mode", e);
                (None, None)
            }
        };

        let orchestrator = Orchestrator::new();

        // Telemetry backend selection is shared with `serve.rs::build_inner`
        // — both must resolve to the same backend or the dashboard reads
        // from a different store than the agent runtime writes to.
        #[cfg(feature = "postgres")]
        let postgres_reachable = postgres_pool.is_some();
        #[cfg(not(feature = "postgres"))]
        let postgres_reachable = false;
        let telemetry_store = shared_telemetry_store(&config, postgres_reachable).await?;

        let state = Arc::new(Self {
            config,
            orchestrator,
            policy_engine,
            memory,
            cost_tracker: Arc::new(CostTracker::with_store(Arc::clone(&telemetry_store))),
            audit_log: Arc::new(AuditLog::new(10_000)),
            node_registry: Arc::new(tokio::sync::RwLock::new(NodeRegistry::default())),
            skill_registry: Arc::new(tokio::sync::RwLock::new(SkillRegistry::new())),
            plugin_registry: Arc::new(tokio::sync::RwLock::new(PluginRegistry::new())),
            lock_manager: Arc::new(LockManager::new()),
            telemetry_store,
            nats,
            jetstream,
            scheduler: None,
            workflow_runtime: None,
            playbook_runtime: None,
            config_path: None,
            auth_token: None,
            agent_jobs: None,
            event_source_manager: None,
            knowledge_service: None,
            identity_store_cell: std::sync::OnceLock::new(),
            agent_registry_cell: std::sync::OnceLock::new(),
            permission_writer_cell: std::sync::OnceLock::new(),
            permission_gate_cell: std::sync::OnceLock::new(),
            capability_gate_cell: std::sync::OnceLock::new(),
        });

        // Hub-level memory metrics: orchestrator task map, memory store
        // (SQLite db file size), process RSS + tokio runtime stats.
        // The knowledge service is set later by `serve.rs` once it's
        // built, since it isn't part of HubState construction. Per-agent
        // metrics (session store, WASM engine, job runner) are wired
        // separately in `serve.rs::build_inner` with `emit_rss=false`.
        {
            let mut metrics = crate::memory_metrics::MemoryMetrics::new();
            metrics.set_orchestrator(Arc::clone(&state.orchestrator));
            metrics.set_memory_store(Arc::clone(&state.memory));
            metrics.spawn();
        }

        install_artifact_store(&state.config).await?;

        tracing::info!("Hub state initialised");
        Ok(state)
    }
}

/// Resolve `[artifacts] backend` to an `ArtifactStore`, failing closed for
/// `s3` and staying tolerant for `fs` — the decision split out from
/// [`install_artifact_store`] so it is unit-testable without the global
/// `OnceCell`. `Ok(None)` means the `fs` store was unavailable but the hub
/// should still boot (tools inline large outputs), mirroring the prior
/// tolerant behavior. Mirrors the `session_store`/`telemetry` factories.
async fn build_artifact_store(
    config: &WorkforceConfig,
) -> eyre::Result<Option<Arc<dyn workforce_agent::artifacts::ArtifactStore>>> {
    use workforce_agent::artifacts::{ArtifactStore, FsArtifactStore};

    match config.artifacts.backend.as_str() {
        "fs" | "" => {
            let root = std::path::PathBuf::from(&config.hub.data_dir).join("artifacts");
            match FsArtifactStore::new(root.clone()) {
                Ok(store) => {
                    tracing::info!(path = %root.display(), "Artifact store ready");
                    Ok(Some(store as Arc<dyn ArtifactStore>))
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %root.display(), "Artifact store unavailable; tools will inline large outputs");
                    Ok(None)
                }
            }
        }
        "s3" => {
            let bucket =
                config.artifacts.s3_bucket.clone().ok_or_else(|| {
                    eyre::eyre!("[artifacts] backend = \"s3\" requires s3_bucket")
                })?;
            let client = build_s3_artifact_client(config).await?;
            client
                .head_bucket()
                .bucket(&bucket)
                .send()
                .await
                .map_err(|e| {
                    eyre::eyre!("[artifacts] S3 bucket `{bucket}` unreachable (fail closed): {e}")
                })?;
            let store = workforce_agent::artifacts::s3_store::S3ArtifactStore::new(
                client,
                bucket.clone(),
                config.artifacts.s3_prefix.clone(),
            );
            tracing::info!(%bucket, prefix = %config.artifacts.s3_prefix, "S3 artifact store ready");
            Ok(Some(Arc::new(store) as Arc<dyn ArtifactStore>))
        }
        other => Err(eyre::eyre!(
            "[artifacts] unknown backend `{other}`; expected \"fs\" or \"s3\""
        )),
    }
}

/// Build the hub-owned S3 client for the artifact store from ONE `SdkConfig`
/// loaded at boot. Optionally wraps the credential chain with a single
/// `sts:AssumeRole` (the hub's *write* identity). Deliberately NOT the
/// per-session `aws::client` factory — that mints tenant assume-role
/// sessions for the read tool, the wrong identity for a writer.
async fn build_s3_artifact_client(config: &WorkforceConfig) -> eyre::Result<aws_sdk_s3::Client> {
    use aws_config::sts::AssumeRoleProvider;
    use aws_config::BehaviorVersion;
    use aws_credential_types::provider::SharedCredentialsProvider;

    let mut loader = aws_config::defaults(BehaviorVersion::latest());
    if let Some(region) = config.artifacts.s3_region.clone() {
        loader = loader.region(aws_config::Region::new(region));
    }
    let base = loader.load().await;
    let sdk_config = match config.artifacts.s3_assume_role_arn.clone() {
        Some(role_arn) => {
            let source = base.credentials_provider().ok_or_else(|| {
                eyre::eyre!("default AWS credentials provider not available for artifact writer")
            })?;
            let provider = AssumeRoleProvider::builder(role_arn)
                .session_name("workforce-artifacts")
                .configure(&base)
                .build_from_provider(SharedCredentialsProvider::new(source))
                .await;
            base.to_builder()
                .credentials_provider(SharedCredentialsProvider::new(provider))
                .build()
        }
        None => base,
    };
    Ok(aws_sdk_s3::Client::new(&sdk_config))
}

/// Build the hub-wide `ArtifactStore` and install it as the global so
/// every tool — across every skill crate — can offload large outputs
/// without each skill needing to thread the store through its own
/// constructors. For the `fs` backend also spawns a TTL sweeper (S3
/// lifecycle rules own GC). Fails closed for `s3` (never silently falls
/// back to disk — split-state is the hazard). Idempotent: only the first
/// `set_global_store` installs; later calls are no-ops via `OnceCell`.
async fn install_artifact_store(config: &WorkforceConfig) -> eyre::Result<()> {
    use workforce_agent::artifacts::set_global_store;

    let Some(store) = build_artifact_store(config).await? else {
        return Ok(());
    };
    set_global_store(store.clone());
    if matches!(config.artifacts.backend.as_str(), "fs" | "") {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                match store.purge_expired() {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(purged = n, "Artifact sweep"),
                    Err(e) => tracing::warn!(error = %e, "Artifact sweep failed"),
                }
            }
        });
    }
    Ok(())
}

/// Resolve `[memory] backend` to a `MemoryStore`, failing closed, using the
/// backend's own `connect_timeout_secs` budget (`[vellev]`'s for vellev,
/// `[weaviate]`'s otherwise). This is the hub-boot entry point; one-shot CLI
/// callers that shouldn't wait a cold-boot budget use
/// [`resolve_memory_store_with_budget`] with a short budget instead.
pub async fn resolve_memory_store(config: &WorkforceConfig) -> eyre::Result<Arc<dyn MemoryStore>> {
    resolve_memory_store_as(config, MemoryOpening::hub(config)).await
}

/// How a process opens memories: how long it waits for the store, and
/// whether its writes are noted for vellev's replica while memories are
/// shadowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MemoryOpening {
    pub(crate) budget: std::time::Duration,
    pub(crate) noting: crate::memory_erasure::Noting,
}

impl MemoryOpening {
    /// The hub's: the configured connect budget, and its writes noted.
    pub(crate) fn hub(config: &WorkforceConfig) -> Self {
        let budget = if config.memory.is_vellev() {
            config.vellev.connect_timeout_secs
        } else {
            config.weaviate.connect_timeout_secs
        };
        Self {
            budget: std::time::Duration::from_secs(budget),
            noting: crate::memory_erasure::Noting::Hub,
        }
    }

    /// A one-shot CLI process's: `budget`, and nothing noted.
    pub(crate) fn cli(budget: std::time::Duration) -> Self {
        Self {
            budget,
            noting: crate::memory_erasure::Noting::Off,
        }
    }
}

/// Resolve `[memory] backend` to a `MemoryStore`, failing closed, bounding the
/// Weaviate readiness wait by `budget`.
///
/// Single source of truth: honors `memory.backend`, keeps the legacy
/// `weaviate.enabled` bridge, defaults to SQLite, and uses an in-memory store
/// only when explicitly named. A configured Weaviate that stays unreachable
/// past `budget` returns `Err` — it never downgrades to SQLite/in-memory.
///
/// For a one-shot CLI process: while memories are shadowed, its writes are
/// not noted for vellev's replica (a noter could die with the process
/// before writing them); the reconcile finds them. Its purges still erase
/// from the replica in the same call.
pub async fn resolve_memory_store_with_budget(
    config: &WorkforceConfig,
    budget: std::time::Duration,
) -> eyre::Result<Arc<dyn MemoryStore>> {
    resolve_memory_store_as(config, MemoryOpening::cli(budget)).await
}

async fn resolve_memory_store_as(
    config: &WorkforceConfig,
    opening: MemoryOpening,
) -> eyre::Result<Arc<dyn MemoryStore>> {
    let MemoryOpening { budget, noting } = opening;
    // Checked here, where memories are opened, rather than when the config
    // is read: a process that never opens them is not stopped by it.
    config.check_memory_backend()?;
    // Before any store is made: opening vellev's makes its first
    // collection, which a shadow refuses its tenant to hold (DW-676).
    crate::promotion::guard_memories(config).await?;
    let backend = config.memory.backend.as_str();
    let effective_backend = if config.weaviate.enabled && backend == "sqlite" {
        "weaviate"
    } else {
        backend
    };

    match effective_backend {
        "weaviate" => weaviate_memory_store(config, budget).await,
        // Weaviate serves; each write is noted for vellev's replica, each
        // recall asked of it in the background, and each purge erases from
        // it in the same call (DW-676).
        workforce_common::settings::SHADOWED => {
            let serving = weaviate_memory_store(config, budget).await?;
            crate::memory_erasure::shadowed(config, serving, noting).await
        }
        "vellev" => {
            tracing::info!(url = %config.vellev.url, tenant = %config.vellev.tenant, has_api_key = config.vellev.api_key.is_some(), "Using vellev memory store");
            let store = crate::embeddings::base_store(config)?;
            ensure_memory_store_ready(store.as_ref(), None, budget, "vellev").await?;
            crate::embeddings::memory_store(config, store, None, budget).await
        }
        "in_memory" => {
            tracing::info!("Using in-memory memory store");
            Ok(Arc::new(InMemoryStore::new()))
        }
        "sqlite" | "" => {
            let global_vault = match config.memory.embedding_provider.as_str() {
                "none" | "" => None,
                _ => crate::vault::open_global(config).await?,
            };
            create_sqlite_store(config, global_vault)
        }
        other => eyre::bail!(
            "[memory] unknown backend {other:?}; expected \"weaviate\", \"vellev\", \"sqlite\", or \"in_memory\""
        ),
    }
}

/// Weaviate's memory store as `[memory]` and `[embeddings]` make it.
async fn weaviate_memory_store(
    config: &WorkforceConfig,
    budget: std::time::Duration,
) -> eyre::Result<Arc<dyn MemoryStore>> {
    let url = if config.weaviate.enabled {
        &config.weaviate.url
    } else {
        "http://localhost:8080"
    };
    tracing::info!(url = %url, has_api_key = config.weaviate.api_key.is_some(), "Using Weaviate memory store");
    let store =
        workforce_memory::weaviate::WeaviateStore::new(url, config.weaviate.api_key.as_deref());
    let backup_dir = std::path::PathBuf::from(&config.hub.data_dir).join("memory-backups");
    let recovery_dir = config
        .weaviate
        .auto_rebuild_on_schema_drift
        .then_some(backup_dir.as_path());
    ensure_memory_store_ready(&store, recovery_dir, budget, "Weaviate").await?;
    crate::embeddings::memory_store(config, Arc::new(store), recovery_dir, budget).await
}

/// How readiness checks are paced: the least time the first check is
/// given however small the budget, and the wait between checks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReadinessPace {
    pub(crate) first_check_floor: std::time::Duration,
    pub(crate) interval: std::time::Duration,
}

impl ReadinessPace {
    const BOOT: ReadinessPace = ReadinessPace {
        first_check_floor: std::time::Duration::from_secs(5),
        interval: std::time::Duration::from_secs(5),
    };
}

/// Wait for the memory store named `label` ("Weaviate", "vellev") to come up
/// and present a usable collection, retrying every 5s until `budget` has
/// elapsed, and no longer. When `backup_dir` is `Some`, schema-drift errors
/// trigger an automatic export-delete-recreate-import rebuild rather than
/// being treated as transient. A collection an operator has to put right
/// ([`workforce_vellev::Unusable`]: refused shape, missing tenant, refused
/// key) fails at once, without waiting out the budget. Fails closed: an
/// exhausted budget returns `Err` so the caller never falls back to a lesser
/// store.
///
/// Each check is given what is left of the budget (the first at least
/// [`ReadinessPace::BOOT`]'s floor), so that a host that accepts connections and
/// never answers cannot hold boot past it. This bounds Weaviate's checks
/// too, which before waited out each request's own 30s timeout however
/// little of the budget was left: an intended fix. A rebuild is never cut
/// short: it has deleted the collection before it re-imports.
pub(crate) async fn ensure_memory_store_ready(
    store: &dyn workforce_memory::admin::MemoryAdmin,
    backup_dir: Option<&std::path::Path>,
    budget: std::time::Duration,
    label: &str,
) -> eyre::Result<()> {
    ensure_memory_store_ready_paced(store, backup_dir, budget, label, ReadinessPace::BOOT).await
}

/// [`ensure_memory_store_ready`], paced by `pace`.
pub(crate) async fn ensure_memory_store_ready_paced(
    store: &dyn workforce_memory::admin::MemoryAdmin,
    backup_dir: Option<&std::path::Path>,
    budget: std::time::Duration,
    label: &str,
    pace: ReadinessPace,
) -> eyre::Result<()> {
    use eyre::WrapErr;
    use tokio::time::Instant;
    use workforce_memory::admin::RecoveryOutcome;

    let interval = pace.interval;
    let deadline = Instant::now() + budget;
    let mut attempt = 0u64;
    loop {
        attempt += 1;
        let left = deadline.saturating_duration_since(Instant::now());
        let allowed = if attempt == 1 {
            left.max(pace.first_check_floor)
        } else {
            left
        };
        let result = match backup_dir {
            Some(dir) => store.ensure_collection_with_recovery(Some(dir)).await,
            None => tokio::time::timeout(allowed, store.ensure_collection())
                .await
                .unwrap_or_else(|_| Err(eyre::eyre!("no answer within {}s", allowed.as_secs())))
                .map(|()| RecoveryOutcome::AlreadyValid),
        };
        let error = match result {
            Ok(RecoveryOutcome::AlreadyValid) => return Ok(()),
            Ok(RecoveryOutcome::Rebuilt {
                exported,
                imported,
                backup_path,
                mismatches,
            }) => {
                tracing::warn!(
                    exported,
                    imported,
                    backup = ?backup_path,
                    mismatches = ?mismatches,
                    "Weaviate schema drift auto-recovered"
                );
                return Ok(());
            }
            Err(e) if e.downcast_ref::<workforce_vellev::Unusable>().is_some() => return Err(e),
            Err(e) => e,
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(error).wrap_err(format!("{label} unavailable after {}s", budget.as_secs()));
        }
        tracing::warn!(
            attempt,
            "{label} not ready ({}), retrying in {}s",
            error,
            interval.as_secs()
        );
        tokio::time::sleep(interval.min(left)).await;
        if Instant::now() >= deadline {
            return Err(error).wrap_err(format!("{label} unavailable after {}s", budget.as_secs()));
        }
    }
}

/// Create a SQLite-backed memory store from the workforce config. Its
/// embedding key is read per request from `global_vault`
/// (`embeddings/memory/api_key`), then the config's, then `OPENAI_API_KEY`.
fn create_sqlite_store(
    config: &WorkforceConfig,
    global_vault: Option<Arc<crate::vault::Vault>>,
) -> eyre::Result<Arc<dyn MemoryStore>> {
    let sqlite_config = SqliteMemoryConfig {
        db_path: config.memory.db_path.clone(),
        embedding_provider: config.memory.embedding_provider.clone(),
        embedding_model: config.memory.embedding_model.clone(),
        embedding_base_url: config.memory.embedding_api_url.clone(),
        ..Default::default()
    };

    tracing::info!(
        path = %sqlite_config.db_path,
        embedding_provider = %sqlite_config.embedding_provider,
        "Using SQLite memory store"
    );

    workforce_common::private_fs::ensure_private_sqlite(std::path::Path::new(
        &sqlite_config.db_path,
    ))?;
    let api_key = crate::credentials::source(
        &crate::credentials::MEMORY_EMBEDDINGS_API_KEY,
        None,
        global_vault,
        config
            .memory
            .embedding_api_key
            .clone()
            .or_else(|| crate::credentials::MEMORY_EMBEDDINGS_API_KEY.from_env()),
    );
    let store = SqliteMemoryStore::open_with_key_source(&sqlite_config, api_key)?;
    Ok(Arc::new(store))
}

/// Resolve `[telemetry] backend` to a `TelemetryStore`, failing closed.
///
/// `postgres_reachable` is the result of the startup pool ping in
/// `HubState::from_config`. A `[telemetry] backend = "postgres"` that can't
/// reach Postgres returns `Err` rather than degrading to SQLite; only a
/// `"sqlite"` backend uses SQLite.
pub async fn create_telemetry_store(
    config: &WorkforceConfig,
    postgres_reachable: bool,
) -> eyre::Result<Arc<dyn TelemetryStore>> {
    let backend = config.telemetry.backend.as_str();

    #[cfg(feature = "postgres")]
    {
        use eyre::WrapErr;
        use workforce_common::backend::{resolve_store_backend, StoreBackend};
        if resolve_store_backend("telemetry", backend, postgres_reachable)?
            == StoreBackend::Postgres
        {
            let pg_cfg = config
                .postgres
                .as_ref()
                .filter(|p| p.enabled)
                .ok_or_else(|| {
                    eyre::eyre!("[telemetry] backend = \"postgres\" but [postgres] is not enabled")
                })?;
            let store = workforce_common::telemetry_postgres::PostgresTelemetryStore::new(pg_cfg)
                .await
                .wrap_err("constructing Postgres telemetry store")?;
            tracing::info!("Using Postgres telemetry store");
            return Ok(store as Arc<dyn TelemetryStore>);
        }
    }
    #[cfg(not(feature = "postgres"))]
    {
        let _ = postgres_reachable;
        match backend {
            "postgres" => eyre::bail!(
                "[telemetry] backend = \"postgres\" but workforce-hub was built without the postgres feature; rebuild with --features postgres or set [telemetry] backend = \"sqlite\""
            ),
            "sqlite" | "" => {}
            other => eyre::bail!(
                "[telemetry] unknown backend {other:?}; expected \"postgres\" or \"sqlite\""
            ),
        }
    }

    let path = std::path::PathBuf::from(&config.hub.data_dir).join("telemetry.db");
    workforce_common::private_fs::ensure_private_sqlite(&path)?;
    tracing::info!(path = %path.display(), "Using SQLite telemetry store");
    Ok(Arc::new(workforce_common::persistence::SqliteStore::open(
        &path,
    )?))
}

/// Process-wide cache for the Postgres telemetry store so the hub and every
/// agent share one connection pool rather than opening one each.
#[cfg(feature = "postgres")]
static SHARED_TELEMETRY_STORE: tokio::sync::OnceCell<Arc<dyn TelemetryStore>> =
    tokio::sync::OnceCell::const_new();

/// Telemetry store, built once per process when Postgres-backed (one shared
/// pool); SQLite stores carry no pool and are built per call. The fail-closed
/// Postgres resolution of [`create_telemetry_store`] is preserved.
pub async fn shared_telemetry_store(
    config: &WorkforceConfig,
    postgres_reachable: bool,
) -> eyre::Result<Arc<dyn TelemetryStore>> {
    #[cfg(feature = "postgres")]
    if config.telemetry.backend == "postgres" {
        return SHARED_TELEMETRY_STORE
            .get_or_try_init(|| create_telemetry_store(config, postgres_reachable))
            .await
            .cloned();
    }
    create_telemetry_store(config, postgres_reachable).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_sqlite_embedder_reads_its_key_from_the_global_vault_on_every_request() {
        use axum::{http::HeaderMap, routing::post, Json, Router};
        use workforce_core::types::AgentId;
        use workforce_memory::types::{Memory, MemoryLayer};

        let seen: Arc<parking_lot::Mutex<Vec<Option<String>>>> = Arc::default();
        let record = Arc::clone(&seen);
        let app = Router::new().route(
            "/v1/embeddings",
            post(move |headers: HeaderMap| async move {
                record.lock().push(
                    headers
                        .get("authorization")
                        .map(|v| v.to_str().unwrap().to_string()),
                );
                Json(serde_json::json!({"data": [{"embedding": [1.0]}]}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let global =
            Arc::new(crate::vault::Vault::open(&dir.path().join("g.enc"), "test-key").unwrap());
        let mut config = WorkforceConfig::default();
        config.memory.db_path = dir.path().join("memory.db").display().to_string();
        config.memory.embedding_provider = "openai".into();
        config.memory.embedding_api_url = url;
        config.memory.embedding_api_key = Some("config-key".into());
        let store = create_sqlite_store(&config, Some(Arc::clone(&global))).unwrap();
        let remember = |text: &'static str| {
            store.remember(Memory::new(
                text,
                MemoryLayer::Semantic,
                AgentId::new("agent"),
            ))
        };

        remember("first").await.unwrap();
        let path = crate::credentials::MEMORY_EMBEDDINGS_API_KEY.path;
        global.set(path, "vault-key").unwrap();
        remember("second").await.unwrap();
        global.set(path, "rotated").unwrap();
        remember("third").await.unwrap();
        assert_eq!(
            *seen.lock(),
            ["config-key", "vault-key", "rotated"].map(|key| Some(format!("Bearer {key}")))
        );
    }

    #[tokio::test]
    async fn weaviate_configured_but_unreachable_fails_closed() {
        let mut config = WorkforceConfig::default();
        config.weaviate.enabled = true;
        config.weaviate.url = "http://127.0.0.1:1".to_string();
        config.weaviate.connect_timeout_secs = 0;
        config.weaviate.auto_rebuild_on_schema_drift = false;
        assert!(resolve_memory_store(&config).await.is_err());
    }

    #[tokio::test]
    async fn vellev_configured_but_unreachable_fails_closed() {
        let mut config = WorkforceConfig::default();
        config.memory.backend = "vellev".to_string();
        config.vellev.url = "http://127.0.0.1:1".to_string();
        config.vellev.api_key = Some("key".to_string());
        config.vellev.tenant = "workforce".to_string();
        assert!(
            resolve_memory_store_with_budget(&config, std::time::Duration::ZERO)
                .await
                .is_err()
        );
    }

    /// A node that accepts connections and never answers holds boot for the
    /// budget, and no longer.
    #[tokio::test]
    async fn a_store_that_never_answers_holds_boot_for_the_budget_and_no_longer() {
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = WorkforceConfig::default();
        config.memory.backend = "vellev".to_string();
        config.vellev.url = format!("http://{}", silent.local_addr().unwrap());
        config.vellev.api_key = Some("key".to_string());
        config.vellev.tenant = "workforce".to_string();
        let store = crate::embeddings::base_store(&config).unwrap();
        let budget = std::time::Duration::from_secs(2);
        let pace = ReadinessPace {
            first_check_floor: std::time::Duration::from_millis(300),
            interval: std::time::Duration::from_millis(300),
        };
        let started = std::time::Instant::now();
        let refused =
            ensure_memory_store_ready_paced(store.as_ref(), None, budget, "vellev", pace).await;
        let took = started.elapsed();
        assert!(refused.is_err());
        assert!(
            took >= budget - std::time::Duration::from_millis(200)
                && took < budget + std::time::Duration::from_millis(700),
            "{took:?}"
        );
        drop(silent);
    }

    /// Weaviate's boot messages are as operators know them.
    #[tokio::test]
    async fn a_weaviate_that_does_not_come_up_is_reported_in_its_own_words() {
        use std::sync::Mutex;
        #[derive(Clone, Default)]
        struct Captured(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let logging = tracing::subscriber::set_default(subscriber);
        let store = workforce_memory::weaviate::WeaviateStore::new("http://127.0.0.1:1", None);
        let pace = ReadinessPace {
            first_check_floor: std::time::Duration::from_millis(200),
            interval: std::time::Duration::from_millis(200),
        };
        let refused = ensure_memory_store_ready_paced(
            &store,
            None,
            std::time::Duration::from_secs(1),
            "Weaviate",
            pace,
        )
        .await
        .unwrap_err();
        drop(logging);
        assert!(
            refused
                .to_string()
                .contains("Weaviate unavailable after 1s"),
            "{refused}"
        );
        let logged = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("Weaviate not ready ("), "{logged}");
    }

    /// A missing tenant is for an operator to create: boot fails at once
    /// rather than waiting out the budget.
    #[tokio::test]
    async fn a_missing_vellev_tenant_fails_boot_at_once() {
        let Some(node) = workforce_vellev::testing::Node::fresh().await else {
            return;
        };
        let mut config = WorkforceConfig::default();
        config.memory.backend = "vellev".to_string();
        config.vellev.url = node.url.clone();
        config.vellev.api_key = Some(node.key.clone());
        config.vellev.tenant = format!("{}-missing", node.tenant);
        let started = std::time::Instant::now();
        let refused = resolve_memory_store_with_budget(&config, std::time::Duration::from_secs(60))
            .await
            .err()
            .expect("refused");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(
            refused.to_string().contains(&config.vellev.tenant),
            "{refused}"
        );
    }

    /// Against the node `VELLEV_TEST_URL` and `VELLEV_TEST_KEY` name, in a
    /// fresh tenant; skipped without them.
    #[tokio::test]
    async fn vellev_backend_resolves_and_makes_its_collection() {
        let Some(node) = workforce_vellev::testing::Node::fresh().await else {
            return;
        };
        let mut config = WorkforceConfig::default();
        config.memory.backend = "vellev".to_string();
        config.vellev.url = node.url.clone();
        config.vellev.api_key = Some(node.key.clone());
        config.vellev.tenant = node.tenant.clone();
        resolve_memory_store_with_budget(&config, std::time::Duration::ZERO)
            .await
            .unwrap();
        crate::embeddings::base_store(&config)
            .unwrap()
            .doctor()
            .await
            .unwrap();
    }

    /// A hub notes the memories it writes for vellev's replica; a one-shot
    /// CLI process never does (its noter could die before writing).
    #[test]
    fn the_hub_opens_memories_noting_and_the_cli_without() {
        use crate::memory_erasure::Noting;
        let config = WorkforceConfig::default();
        assert_eq!(MemoryOpening::hub(&config).noting, Noting::Hub);
        let budget = std::time::Duration::from_secs(3);
        assert_eq!(
            MemoryOpening::cli(budget),
            MemoryOpening {
                budget,
                noting: Noting::Off
            }
        );
    }

    #[tokio::test]
    async fn unknown_memory_backend_fails_closed() {
        let mut config = WorkforceConfig::default();
        config.memory.backend = "mongodb".to_string();
        assert!(resolve_memory_store(&config).await.is_err());
    }

    /// The hub opens memories, so it is refused a memory backend it cannot
    /// use, by name and for what it lacks, never given a lesser store:
    /// "shadow" with nothing it needs; with everything it needs it is
    /// opened (here failing only for want of a Weaviate to reach).
    #[tokio::test]
    async fn a_shadowed_memory_backend_is_refused_only_for_what_it_lacks() {
        let mut config = WorkforceConfig::default();
        config.memory.backend = "shadow".to_string();
        let refused = resolve_memory_store(&config)
            .await
            .err()
            .expect("refused")
            .to_string();
        assert!(
            refused.contains("[memory] backend = \"shadow\" needs [vellev] api_key"),
            "{refused}"
        );
        config.weaviate.enabled = true;
        config.vellev.api_key = Some("k".into());
        config.vellev.tenant = "acme".into();
        config.embeddings.provider = workforce_common::settings::EmbeddingsProvider::Tei;
        let refused = resolve_memory_store(&config)
            .await
            .err()
            .expect("refused")
            .to_string();
        assert!(
            refused.contains("[memory] backend = \"shadow\" needs [postgres] enabled"),
            "{refused}"
        );
        config.postgres = Some(workforce_common::postgres::PostgresConfig {
            enabled: true,
            url: Some("postgres://hub@db:5432/workforce".into()),
            ..Default::default()
        });
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        config.weaviate.url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let failed = format!(
            "{:#}",
            resolve_memory_store_with_budget(&config, std::time::Duration::ZERO)
                .await
                .err()
                .expect("no Weaviate to reach")
        );
        assert!(!failed.contains("[memory] backend"), "{failed}");
        // Postgres is not there either: promotion's guard (DW-676, PR 11)
        // asks it first, and refuses rather than open on facts unread. The
        // Weaviate branch, with Postgres there, is
        // `promotion::tests::postgres::a_shadow_with_postgres_there_fails_on_weaviate_alone`.
        assert!(
            failed.contains(
                "whether memories were promoted to vellev cannot be read, so they are not opened from \"shadow\""
            ),
            "{failed}"
        );
    }

    #[tokio::test]
    async fn in_memory_backend_is_honored() {
        let mut config = WorkforceConfig::default();
        config.memory.backend = "in_memory".to_string();
        assert!(resolve_memory_store(&config).await.is_ok());
    }

    #[tokio::test]
    async fn default_config_resolves_sqlite_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = dir.path().to_string_lossy().into_owned();
        config.memory.db_path = dir.path().join("memory.db").to_string_lossy().into_owned();
        assert!(resolve_memory_store(&config).await.is_ok());
    }

    #[tokio::test]
    async fn telemetry_postgres_unreachable_fails_closed() {
        let mut config = WorkforceConfig::default();
        config.telemetry.backend = "postgres".to_string();
        assert!(create_telemetry_store(&config, false).await.is_err());
    }

    #[tokio::test]
    async fn telemetry_defaults_to_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = dir.path().to_string_lossy().into_owned();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        assert!(create_telemetry_store(&config, false).await.is_ok());
    }

    // End-to-end startup guard: the default (sqlite, no Postgres, Weaviate
    // disabled) config must still boot through the real `from_config` chain —
    // i.e. the fail-closed resolvers don't reject a normal install.
    #[tokio::test]
    async fn from_config_boots_on_default_sqlite_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = dir.path().to_string_lossy().into_owned();
        config.memory.db_path = dir.path().join("memory.db").to_string_lossy().into_owned();
        HubState::from_config(config)
            .await
            .expect("hub must boot on a default sqlite config");
    }

    // SQLite telemetry carries no pool, so the shared wrapper resolves it per
    // call without touching the process-wide Postgres cell.
    #[tokio::test]
    async fn shared_telemetry_store_resolves_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = dir.path().to_string_lossy().into_owned();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        assert!(shared_telemetry_store(&config, false).await.is_ok());
    }

    // Default (fs) backend resolves a usable filesystem store.
    #[tokio::test(flavor = "multi_thread")]
    async fn artifact_store_fs_backend_installs_ok() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = dir.path().to_string_lossy().into_owned();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        assert_eq!(config.artifacts.backend, "fs");
        let store = build_artifact_store(&config).await.unwrap();
        assert!(store.is_some(), "fs backend must yield a store");
    }

    // `Arc<dyn ArtifactStore>` isn't `Debug`, so the `Result`'s `Ok` arm can't
    // go through `unwrap_err`; this extracts the error message directly and
    // fails the test if the call unexpectedly succeeded.
    fn expect_build_err(
        result: eyre::Result<Option<Arc<dyn workforce_agent::artifacts::ArtifactStore>>>,
    ) -> String {
        match result {
            Ok(_) => panic!("expected build_artifact_store to fail closed, got Ok"),
            Err(e) => e.to_string(),
        }
    }

    // An unknown backend fails closed with the expected message — guards the
    // s3 fail-closed test below from greening on the wrong arm.
    #[tokio::test(flavor = "multi_thread")]
    async fn artifact_store_unknown_backend_fails_closed() {
        let mut config = WorkforceConfig::default();
        config.artifacts.backend = "gcs".to_string();
        let err = expect_build_err(build_artifact_store(&config).await);
        assert!(err.contains("unknown backend"), "{err}");
    }

    // The s3 backend with an unreachable bucket must fail closed — never
    // silently fall back to disk. Static dummy creds + disabled IMDS force a
    // fast HeadBucket failure with no AWS account; the assertion confirms it
    // tripped the reachability path, not the unknown-backend arm.
    #[tokio::test(flavor = "multi_thread")]
    async fn artifact_store_s3_unreachable_fails_closed() {
        std::env::set_var("AWS_EC2_METADATA_DISABLED", "true");
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE");
        std::env::set_var(
            "AWS_SECRET_ACCESS_KEY",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        );
        let mut config = WorkforceConfig::default();
        config.artifacts.backend = "s3".to_string();
        config.artifacts.s3_bucket =
            Some("workforce-artifacts-does-not-exist-zzz-9f2a1b".to_string());
        config.artifacts.s3_region = Some("us-east-1".to_string());

        let err = expect_build_err(build_artifact_store(&config).await);
        assert!(
            err.contains("unreachable") && err.contains("fail closed"),
            "expected a fail-closed reachability error, got: {err}"
        );
    }

    // The s3 backend without a bucket fails closed before touching AWS.
    #[tokio::test(flavor = "multi_thread")]
    async fn artifact_store_s3_requires_bucket() {
        let mut config = WorkforceConfig::default();
        config.artifacts.backend = "s3".to_string();
        config.artifacts.s3_bucket = None;
        let err = expect_build_err(build_artifact_store(&config).await);
        assert!(err.contains("s3_bucket"), "{err}");
    }
}
