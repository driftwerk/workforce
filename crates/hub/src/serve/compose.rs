//! How an agent's service is built and which tools it registers once it
//! holds its own `Arc`: the constructors, `build_inner`, and the `register_*`
//! methods the registry calls after construction.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::Result;
use tokio::sync::RwLock;
use tracing::{info, warn};

use workforce_agent::attachments::{Describer, Transcriber};
use workforce_agent::llm::failover::{FailoverConfig, FailoverProvider, HealthTracker};
use workforce_agent::llm::pool::PooledProvider;
use workforce_agent::llm::provider::{bedrock_catalog, create_named_provider_async, ProviderKind};
use workforce_agent::runner::{default_tools, InjectedMessage, ToolSecrets};
use workforce_common::cost::CostTracker;
use workforce_common::identity::AgentIdentity;
use workforce_common::locking::LockManager;
use workforce_common::persistence::{TaskTelemetryRecord, ToolTelemetryRecord};
use workforce_common::session::SessionStore;
use workforce_common::settings::{
    LlmConfig, LlmImageGenerationConfig, LlmSpeechConfig, LlmVideoGenerationConfig, WorkforceConfig,
};
use workforce_common::telemetry::TelemetryStore;
use workforce_common::text::truncate_with_ellipsis;
use workforce_core::types::AgentId;
use workforce_policy::engine::PolicyEngine;

use crate::memory_tool::MemoryTool;
use crate::notes_tool::NotesTool;
use crate::reaction_tool::{ReactionRemoveTool, ReactionTool};
use crate::reminder_tool::ReminderTool;
use crate::scheduler::Scheduler;
use crate::scheduler_tool::SchedulerTool;
use crate::vault::Vault;
use crate::vault_tool::{
    VaultDeleteTool, VaultGetTool, VaultInspectTool, VaultListTool, VaultSetTool,
};
use crate::workflow_runtime::shared_workflow_store;
use workforce_common::channels::SurfaceRegistry;
use workforce_skills::adapters::image_view::ViewAttachmentTool;
use workforce_skills::adapters::messaging::{
    MessagingChannelInfoTool, MessagingDeleteTool, MessagingEditTool, MessagingEmojiListTool,
    MessagingListPinsTool, MessagingMemberInfoTool, MessagingPinTool, MessagingReadThreadTool,
    MessagingReadTool, MessagingSendFileTool, MessagingSendTool, MessagingUnpinTool,
    MessagingUploadFileTool,
};
use workforce_skills::adapters::user_photo::GetUserPhotoTool;

use tracing::error;
use workforce_policy::rules::PolicyConfig;

use super::BUILTIN_ALLOW_ALL;
use super::{
    agent_provider, describer_config, merged_effective_llm, resolved_failover, to_failover_retry,
    transcriber_config, ActiveRunnerHandle, AgentService, IsolatedRun,
};

/// The subscription whose login a coding engine runs on, if any.
fn engine_login(
    kind: workforce_common::settings::CodingEngineKind,
) -> Option<workforce_common::subscription::SubscriptionKind> {
    use workforce_common::settings::CodingEngineKind;
    match kind {
        CodingEngineKind::ClaudeCode => {
            Some(workforce_common::subscription::SubscriptionKind::Claude)
        }
        CodingEngineKind::Codex => Some(workforce_common::subscription::SubscriptionKind::ChatGpt),
        _ => None,
    }
}

/// Build the `OwnerResolverFn` closure for vault tools — turns a
/// `VaultEntry::created_by`/`updated_by` tag (`user:<uuid>`, `system:<job>`,
/// `cli`, `legacy`, `agent:<id>`, `rest:owner-token`) into a human-readable
/// display name. Only `user:<uuid>` requires the identity store; other tags
/// get a fixed label.
fn make_owner_resolver(
    identity_store: Arc<
        parking_lot::RwLock<Option<Arc<dyn workforce_identity::store::IdentityStore>>>,
    >,
) -> crate::vault_tool::OwnerResolverFn {
    Arc::new(move |actor: &str| match actor {
        "cli" => Some("Hub CLI".to_string()),
        "rest:owner-token" => Some("Admin (REST owner token)".to_string()),
        "legacy" => Some("Pre-2026 migration".to_string()),
        a if a.starts_with("system:") => {
            Some(format!("System ({})", a.trim_start_matches("system:")))
        }
        a if a.starts_with("agent:") => Some(format!("Agent ({})", a.trim_start_matches("agent:"))),
        a if a.starts_with("user:") => {
            let uid = a.trim_start_matches("user:");
            let store = identity_store.read().clone()?;
            uuid::Uuid::parse_str(uid)
                .ok()
                .and_then(|u| store.get_by_id(u).ok().flatten())
                .map(|i| i.display_name)
        }
        _ => None,
    })
}

/// Load the vision tier registry from embedded defaults, then layer the
/// optional `{workspace}/vision_tiers.toml` override on top.
fn load_vision_tier_registry(
    workspace: &Path,
) -> Arc<workforce_agent::llm::vision_tiers::VisionTierRegistry> {
    use workforce_agent::llm::vision_tiers::VisionTierRegistry;
    let mut reg = VisionTierRegistry::from_defaults().unwrap_or_else(|e| {
        warn!(error = %e, "Failed to load default vision tiers; using empty registry");
        VisionTierRegistry::from_toml("").expect("empty toml parses")
    });
    let override_path = workspace.join("vision_tiers.toml");
    if let Ok(Some(toml_str)) = workforce_common::config_files::read(&override_path) {
        if let Err(e) = reg.merge_toml(&toml_str) {
            warn!(error = %e, path = %override_path.display(), "vision_tiers.toml override failed to merge");
        }
    }
    Arc::new(reg)
}

/// The agent's describer from `effective_llm.vision`: ready, absent (vision
/// unconfigured or switched off), or misconfigured, with a warning naming
/// the model spec that could not be routed.
///
/// Built once at agent startup so the LRU description cache in the underlying
/// adapter spans turns, deduplicating re-uploaded screenshots across the
/// whole conversation.
fn build_agent_describer(
    effective_llm: &LlmConfig,
    vision_tiers: &Arc<workforce_agent::llm::vision_tiers::VisionTierRegistry>,
    cost_tracker: &Arc<CostTracker>,
    agent_id: &str,
    plans: &crate::subscription::PlanLogins,
    vault: Option<Arc<crate::vault::Vault>>,
    global_vault: Option<Arc<crate::vault::Vault>>,
) -> Describer {
    let Some(cfg) = describer_config(effective_llm) else {
        return Describer::Unconfigured;
    };
    match crate::vision::HubVisionBuilder::new(cfg.clone(), Arc::clone(vision_tiers))
        .with_cost_attribution(Arc::clone(cost_tracker), agent_id.to_string())
        .with_plans(plans.clone())
        .with_vaults(vault, global_vault)
        .build()
    {
        Ok(llm) => Describer::Ready(llm, cfg),
        Err(e) => {
            warn!(
                agent = agent_id,
                model_spec = %cfg.model,
                error = %e,
                "[llm.vision] is misconfigured; images are not described until it is fixed and the agent restarted"
            );
            Describer::Misconfigured(e.to_string())
        }
    }
}

/// Load the transcription tier registry from embedded defaults, then layer
/// the optional `{workspace}/transcription_tiers.toml` override on top.
fn load_transcription_tier_registry(
    workspace: &Path,
) -> Arc<workforce_agent::llm::transcription_tiers::TranscriptionTierRegistry> {
    use workforce_agent::llm::transcription_tiers::TranscriptionTierRegistry;
    let mut reg = TranscriptionTierRegistry::from_defaults().unwrap_or_else(|e| {
        warn!(error = %e, "Failed to load default transcription tiers; using empty registry");
        TranscriptionTierRegistry::from_toml("").expect("empty toml parses")
    });
    let override_path = workspace.join("transcription_tiers.toml");
    if let Ok(Some(toml_str)) = workforce_common::config_files::read(&override_path) {
        if let Err(e) = reg.merge_toml(&toml_str) {
            warn!(error = %e, path = %override_path.display(), "transcription_tiers.toml override failed to merge");
        }
    }
    Arc::new(reg)
}

/// The agent's transcriber from `effective_llm.transcription`: ready, absent
/// (transcription unconfigured or switched off), or misconfigured, with a
/// warning naming the model spec that could not be routed. Built once at
/// agent startup so the LRU transcript cache spans turns.
fn build_agent_transcriber(
    effective_llm: &LlmConfig,
    transcription_tiers: &Arc<workforce_agent::llm::transcription_tiers::TranscriptionTierRegistry>,
    cost_tracker: &Arc<CostTracker>,
    agent_id: &str,
    vault: Option<Arc<crate::vault::Vault>>,
    global_vault: Option<Arc<crate::vault::Vault>>,
) -> Transcriber {
    let Some(cfg) = transcriber_config(effective_llm) else {
        return Transcriber::Unconfigured;
    };
    match crate::transcription::HubTranscriptionBuilder::new(
        cfg.clone(),
        Arc::clone(transcription_tiers),
    )
    .with_cost_attribution(Arc::clone(cost_tracker), agent_id.to_string())
    .with_vaults(vault, global_vault)
    .build()
    {
        Ok(provider) => Transcriber::Ready(provider, cfg),
        Err(e) => {
            warn!(
                agent = agent_id,
                model_spec = %cfg.model,
                error = %e,
                "[llm.transcription] is misconfigured; audio and video are saved, not transcribed, until it is fixed and the agent restarted"
            );
            Transcriber::Misconfigured(e.to_string())
        }
    }
}

/// Resources a hub builds once and every agent service shares. Grouped so the
/// builder stays inside the argument budget and a new agent path cannot silently
/// forget one — a second workflow runtime is what made one cron tick fire five
/// times, each under a different agent.
#[derive(Clone)]
pub struct SharedAgentResources {
    pub knowledge: Option<Arc<crate::knowledge::KnowledgeService>>,
    pub local_agents: Option<Arc<crate::hub_core::local_agent_registry::LocalAgentRegistry>>,
    pub surface_registry: Option<Arc<tokio::sync::RwLock<SurfaceRegistry>>>,
    pub workflow_runtime: Option<Arc<dyn crate::hosts::WorkflowHost>>,
    /// What every agent's subscription providers share, built once at boot
    /// ([`crate::subscription::Subscriptions::boot`]).
    pub subscriptions: crate::subscription::Subscriptions,
}

/// Longest startup will wait for every credential probe combined.
///
/// Startup must finish whether or not the outside world is answering, so this
/// is a ceiling on the whole step rather than a per-call timeout.
const CREDENTIAL_PROBE_BUDGET: std::time::Duration = std::time::Duration::from_secs(45);

/// Where an agent's edits land, for its compile gate: the roots its own
/// tools write under, and the `coding.session` workspaces of its projects,
/// built in the environment their rounds run in.
fn compile_gate_roots(
    config: &WorkforceConfig,
    agent_id: &str,
    tool_paths: &[PathBuf],
    runner: Option<&workforce_agent::tools::exec_runner::RunnerEndpoint>,
) -> Vec<crate::code::compile_gate::GateRoot> {
    use crate::code::compile_gate::GateRoot;
    let work = crate::hub_core::layout::WorkRoots::from_hub(&config.hub);
    let round_env = crate::coding_session_tool::DriverProfile::from(&config.coding_session)
        .setup_runner(runner)
        .env;
    let sessions = std::iter::once(crate::hub_core::layout::projects_root(
        &crate::coding_sandbox::absolutise_data_dir(work.data_dir),
        agent_id,
    ))
    .chain(
        work.hot
            .map(|hot| crate::coding_sandbox::absolutise_data_dir(hot).join(agent_id)),
    );
    tool_paths
        .iter()
        .map(|path| GateRoot::agent(path))
        .chain(sessions.map(|path| GateRoot::coding_session(&path, round_env.clone())))
        .collect()
}

/// Building an [`AgentService`]: the constructors the hub, the registry and
/// the tests compose a service with.
#[async_trait::async_trait]
pub trait Compose {
    /// Build the agent service from config (without scheduler tool).
    async fn from_config(
        config: WorkforceConfig,
        config_path: Option<PathBuf>,
    ) -> Result<Arc<Self>>;

    /// Build the agent service with a scheduler (registers the scheduler tool).
    async fn from_config_with_scheduler(
        config: WorkforceConfig,
        config_path: Option<PathBuf>,
    ) -> Result<(Arc<Self>, Arc<Scheduler>)>;

    /// Build with scheduler AND a delivery callback for pushing results to clients.
    async fn from_config_with_scheduler_and_delivery(
        config: WorkforceConfig,
        config_path: Option<PathBuf>,
        delivery: crate::scheduler::DeliveryCallback,
    ) -> Result<(Arc<Self>, Arc<Scheduler>)>;

    /// Build with scheduler, delivery callback, and reaction service.
    async fn from_config_with_scheduler_delivery_and_reactions(
        config: WorkforceConfig,
        config_path: Option<PathBuf>,
        delivery: crate::scheduler::DeliveryCallback,
        global_vault: Option<Arc<Vault>>,
        start_root_indexing: bool,
        subscriptions: crate::subscription::Subscriptions,
    ) -> Result<(Arc<Self>, Arc<Scheduler>)>;

    /// Build an additional agent that owns its OWN scheduler, mirroring the
    /// default agent's double-build: a first pass seeds the scheduler, then a
    /// second pass (sharing the first's knowledge service and surface registry,
    /// DW-479) registers the scheduler/reminder tools and the consolidation
    /// job against it and adopts it.
    #[allow(clippy::too_many_arguments)]
    async fn build_for_agent_with_scheduler(
        config: WorkforceConfig,
        config_path: Option<PathBuf>,
        agent_config: &workforce_common::settings::AgentConfig,
        delivery: crate::scheduler::DeliveryCallback,
        lock_manager: Arc<LockManager>,
        shared_knowledge: Option<Arc<crate::knowledge::KnowledgeService>>,
        shared_local_agents: Option<Arc<crate::hub_core::local_agent_registry::LocalAgentRegistry>>,
        shared_workflow_runtime: Option<Arc<dyn crate::hosts::WorkflowHost>>,
        global_vault: Option<Arc<Vault>>,
        subscriptions: crate::subscription::Subscriptions,
    ) -> Result<(Arc<Self>, Arc<Scheduler>)>;

    /// Build an `AgentService` for a specific agent defined in config.
    ///
    /// If `agent_config` is `None`, falls back to the legacy single-agent
    /// setup (workspace = `{data_dir}/workspace`, agent_id = "default").
    ///
    /// `shared_knowledge` — if provided, reuses an existing `KnowledgeService`
    /// (and skips FS-watcher / startup-indexing, which the caller should manage).
    ///
    /// `shared_local_agents` — if provided, reuses an existing `LocalAgentRegistry`
    /// so all agents share the same registry as the gateway (DW-324).
    async fn build_for_agent(
        config: WorkforceConfig,
        config_path: Option<PathBuf>,
        agent_config: Option<&workforce_common::settings::AgentConfig>,
        scheduler: Option<Arc<Scheduler>>,
        lock_manager: Arc<LockManager>,
        shared_knowledge: Option<Arc<crate::knowledge::KnowledgeService>>,
        shared_local_agents: Option<Arc<crate::hub_core::local_agent_registry::LocalAgentRegistry>>,
        shared_workflow_runtime: Option<Arc<dyn crate::hosts::WorkflowHost>>,
        global_vault: Option<Arc<Vault>>,
        subscriptions: crate::subscription::Subscriptions,
    ) -> Result<Arc<Self>>;
}

#[async_trait::async_trait]
impl Compose for AgentService {
    async fn from_config(
        config: WorkforceConfig,
        config_path: Option<PathBuf>,
    ) -> Result<Arc<Self>> {
        let lock_manager = Arc::new(LockManager::new());
        let subscriptions = crate::subscription::Subscriptions::boot(&config, None);
        build(
            config,
            config_path,
            None,
            lock_manager,
            None,
            true,
            subscriptions,
        )
        .await
    }

    async fn from_config_with_scheduler(
        config: WorkforceConfig,
        config_path: Option<PathBuf>,
    ) -> Result<(Arc<Self>, Arc<Scheduler>)> {
        let user_tz = config.user_timezone();
        let lock_manager = Arc::new(LockManager::new());
        let subscriptions = crate::subscription::Subscriptions::boot(&config, None);

        // Pass 1 seeds the scheduler.
        let service = build(
            config.clone(),
            config_path.clone(),
            None,
            Arc::clone(&lock_manager),
            None,
            true,
            subscriptions.clone(),
        )
        .await?;

        // Create scheduler with the configured timezone
        let scheduler = Arc::new(Scheduler::new_with_timezone(Arc::clone(&service), user_tz));

        // Pass 2 registers the scheduler tool and adopts the scheduler.
        let service_with_tool = build(
            config,
            config_path,
            Some(Arc::clone(&scheduler)),
            lock_manager,
            None,
            true,
            subscriptions,
        )
        .await?;

        scheduler.set_owner(Arc::downgrade(&service_with_tool));
        Ok((service_with_tool, scheduler))
    }

    async fn from_config_with_scheduler_and_delivery(
        config: WorkforceConfig,
        config_path: Option<PathBuf>,
        delivery: crate::scheduler::DeliveryCallback,
    ) -> Result<(Arc<Self>, Arc<Scheduler>)> {
        let subscriptions = crate::subscription::Subscriptions::boot(&config, None);
        Self::from_config_with_scheduler_delivery_and_reactions(
            config,
            config_path,
            delivery,
            None,
            true,
            subscriptions,
        )
        .await
    }

    async fn from_config_with_scheduler_delivery_and_reactions(
        config: WorkforceConfig,
        config_path: Option<PathBuf>,
        delivery: crate::scheduler::DeliveryCallback,
        global_vault: Option<Arc<Vault>>,
        start_root_indexing: bool,
        subscriptions: crate::subscription::Subscriptions,
    ) -> Result<(Arc<Self>, Arc<Scheduler>)> {
        let user_tz = config.user_timezone();
        let lock_manager = Arc::new(LockManager::new());

        // Pass 1 seeds the scheduler.
        let service = build(
            config.clone(),
            config_path.clone(),
            None,
            Arc::clone(&lock_manager),
            global_vault.clone(),
            start_root_indexing,
            subscriptions.clone(),
        )
        .await?;

        // Create scheduler with delivery callback and configured timezone
        let scheduler = Arc::new(Scheduler::with_delivery_and_timezone(
            Arc::clone(&service),
            delivery,
            user_tz,
        ));

        // Rebuild with the scheduler tool registered, reusing the first pass's
        // knowledge service (no duplicate indexing) and surface registry (DW-479).
        // The scheduler then adopts this second service, so its jobs run against
        // the service that owns it rather than the bootstrap pass.
        let shared_ks = service.knowledge_service().cloned();
        let shared_la = service.local_agents.clone();
        let shared_sr = Arc::clone(&service.surface_registry);
        let agent_config = config.default_agent().cloned();
        let service_with_tool = build_inner(
            config,
            config_path,
            agent_config.as_ref(),
            Some(Arc::clone(&scheduler)),
            lock_manager,
            SharedAgentResources {
                knowledge: shared_ks,
                local_agents: shared_la,
                surface_registry: Some(shared_sr),
                // Never the first pass's runtime: that service was built without
                // a scheduler, and a runtime bound to it can register no cron at
                // all. This second pass mints the runtime every other agent shares.
                workflow_runtime: None,
                subscriptions,
            },
            global_vault,
            true,
        )
        .await?;

        scheduler.set_owner(Arc::downgrade(&service_with_tool));
        Ok((service_with_tool, scheduler))
    }

    #[allow(clippy::too_many_arguments)]
    async fn build_for_agent_with_scheduler(
        config: WorkforceConfig,
        config_path: Option<PathBuf>,
        agent_config: &workforce_common::settings::AgentConfig,
        delivery: crate::scheduler::DeliveryCallback,
        lock_manager: Arc<LockManager>,
        shared_knowledge: Option<Arc<crate::knowledge::KnowledgeService>>,
        shared_local_agents: Option<Arc<crate::hub_core::local_agent_registry::LocalAgentRegistry>>,
        shared_workflow_runtime: Option<Arc<dyn crate::hosts::WorkflowHost>>,
        global_vault: Option<Arc<Vault>>,
        subscriptions: crate::subscription::Subscriptions,
    ) -> Result<(Arc<Self>, Arc<Scheduler>)> {
        let user_tz = config.user_timezone();
        let service = build_inner(
            config.clone(),
            config_path.clone(),
            Some(agent_config),
            None,
            Arc::clone(&lock_manager),
            SharedAgentResources {
                knowledge: shared_knowledge.clone(),
                local_agents: shared_local_agents.clone(),
                surface_registry: None,
                workflow_runtime: shared_workflow_runtime.clone(),
                subscriptions: subscriptions.clone(),
            },
            global_vault.clone(),
            true,
        )
        .await?;
        let scheduler = Arc::new(Scheduler::with_delivery_and_timezone(
            Arc::clone(&service),
            delivery,
            user_tz,
        ));
        let shared_sr = Arc::clone(&service.surface_registry);
        let service_with_tool = build_inner(
            config,
            config_path,
            Some(agent_config),
            Some(Arc::clone(&scheduler)),
            lock_manager,
            SharedAgentResources {
                knowledge: shared_knowledge,
                local_agents: shared_local_agents,
                surface_registry: Some(shared_sr),
                // Only ever the runtime handed in, never the first pass's own:
                // that service has no scheduler, so a runtime bound to it can
                // register no cron.
                workflow_runtime: shared_workflow_runtime,
                subscriptions,
            },
            global_vault,
            true,
        )
        .await?;
        scheduler.set_owner(Arc::downgrade(&service_with_tool));
        Ok((service_with_tool, scheduler))
    }

    async fn build_for_agent(
        config: WorkforceConfig,
        config_path: Option<PathBuf>,
        agent_config: Option<&workforce_common::settings::AgentConfig>,
        scheduler: Option<Arc<Scheduler>>,
        lock_manager: Arc<LockManager>,
        shared_knowledge: Option<Arc<crate::knowledge::KnowledgeService>>,
        shared_local_agents: Option<Arc<crate::hub_core::local_agent_registry::LocalAgentRegistry>>,
        shared_workflow_runtime: Option<Arc<dyn crate::hosts::WorkflowHost>>,
        global_vault: Option<Arc<Vault>>,
        subscriptions: crate::subscription::Subscriptions,
    ) -> Result<Arc<Self>> {
        build_inner(
            config,
            config_path,
            agent_config,
            scheduler,
            lock_manager,
            SharedAgentResources {
                knowledge: shared_knowledge,
                local_agents: shared_local_agents,
                surface_registry: None, // each agent gets its own surface registry
                workflow_runtime: shared_workflow_runtime,
                subscriptions,
            },
            global_vault,
            true,
        )
        .await
    }
}

/// Internal builder — uses the default [[agents]] entry when present.
pub(super) async fn build(
    config: WorkforceConfig,
    config_path: Option<PathBuf>,
    scheduler: Option<Arc<Scheduler>>,
    lock_manager: Arc<LockManager>,
    global_vault: Option<Arc<Vault>>,
    start_root_indexing: bool,
    subscriptions: crate::subscription::Subscriptions,
) -> Result<Arc<AgentService>> {
    let agent_config = config.default_agent().cloned();
    build_inner(
        config,
        config_path,
        agent_config.as_ref(),
        scheduler,
        lock_manager,
        // Single-agent path: owns its knowledge service, local agent
        // registry, surface registry and the only workflow runtime.
        SharedAgentResources {
            knowledge: None,
            local_agents: None,
            surface_registry: None,
            workflow_runtime: None,
            subscriptions,
        },
        global_vault,
        start_root_indexing,
    )
    .await
}

/// Shared builder implementation.
///
/// When `shared_knowledge` is `Some`, the service reuses that instance
/// and skips FS-watcher / startup-indexing (the caller manages those).
pub(super) async fn build_inner(
    config: WorkforceConfig,
    config_path: Option<PathBuf>,
    agent_config: Option<&workforce_common::settings::AgentConfig>,
    scheduler: Option<Arc<Scheduler>>,
    lock_manager: Arc<LockManager>,
    shared: SharedAgentResources,
    global_vault: Option<Arc<Vault>>,
    start_root_indexing: bool,
) -> Result<Arc<AgentService>> {
    let SharedAgentResources {
        knowledge: shared_knowledge,
        local_agents: shared_local_agents,
        surface_registry: shared_surface_registry,
        workflow_runtime: shared_workflow_runtime,
        subscriptions,
    } = shared;
    let setup = Setup::resolve(&config, agent_config).await?;

    let credentials =
        Credentials::open(&config, &setup, agent_config, global_vault, subscriptions).await?;

    let llm_stack = LlmStack::build(&setup, &credentials).await?;

    let toolbox = Toolbox::new(&config, &setup, &credentials);

    let stores = Stores::open(&config, &setup).await?;

    register_memory_tools(&config, &setup, &stores, &toolbox, agent_config);

    let compile_gate = register_code_tools(&config, &setup, &toolbox);

    register_github_tools(&setup, &credentials, &toolbox);

    register_aws_tools(&config, &toolbox).await;

    let knowledge_service_arc = register_knowledge(
        &config,
        &setup,
        &credentials,
        &toolbox,
        &stores,
        shared_knowledge,
        start_root_indexing,
    );

    let admin = Admin::new();

    register_document_store_tools(&config, &toolbox, &admin).await;

    prepare_scheduler(&setup, &toolbox, &admin, scheduler.as_ref()).await?;

    let vaults = Vaults::register(&config, &credentials, &admin, &toolbox);

    let surface_registry = shared_surface_registry
        .unwrap_or_else(|| Arc::new(tokio::sync::RwLock::new(SurfaceRegistry::new())));
    register_reaction_tools(&toolbox, &surface_registry);

    install_media_tools(&setup, &credentials, &toolbox);

    register_messaging_tools(&setup, &stores, &toolbox, &admin, &surface_registry);

    // Per-agent memory metrics. The hub-level instance in `state.rs` already
    // emits process RSS, so disable it here to avoid N copies per tick when
    // the hub runs multiple agents.
    let mut memory_metrics = crate::memory_metrics::MemoryMetrics::new().with_rss(false);
    memory_metrics.add_session_store(setup.agent_id.as_str(), Arc::clone(&stores.sessions));
    if let Some(ref ks) = knowledge_service_arc {
        memory_metrics.set_knowledge_service(Arc::clone(ks));
    }

    register_artifact_tools(&config, &setup, &stores, &toolbox).await;

    let overrides = Overrides::load(&setup, &credentials, &llm_stack).await?;

    probe_credentials(&llm_stack).await;

    migrate_vault_config(&setup, &credentials);

    let skills = Skills::register(
        &config,
        &setup,
        &credentials,
        &llm_stack,
        &toolbox,
        &stores,
        &admin,
        &vaults,
        &overrides,
        &mut memory_metrics,
    )
    .await?;

    let (policy, adapter_loader) =
        install_policy(&config, &credentials, &toolbox, &admin, &skills)?;

    register_telemetry_tools(
        &config,
        &setup,
        &credentials,
        &toolbox,
        &skills,
        &vaults,
        &config_path,
        &lock_manager,
    )
    .await;

    let check_signer = register_heartbeat_tools(&config, &setup, &toolbox);

    let jobs = JobRunning::start(
        &setup,
        &toolbox,
        &stores,
        &admin,
        &skills,
        &surface_registry,
        &mut memory_metrics,
    );

    let trace_writer = build_trace_writer(&config, &setup);

    let media = Media::build(
        &config,
        &setup,
        &credentials,
        &toolbox,
        &stores,
        &skills,
        &surface_registry,
    );

    let Setup {
        agent_id,
        workspace,
        identity,
        config_text,
        effective_llm,
        config_equivalences_cell,
        provider_kind,
        tier_key,
        shared_config,
        admission,
        resolved,
        resolved_at_build,
        original_default_model,
        current_default_model,
        ..
    } = setup;
    let Credentials {
        vault,
        global_vault,
        team_store_handle,
        owner_override,
        logins,
        providers,
        subscriptions,
        ..
    } = credentials;
    let LlmStack {
        pool,
        llm,
        health_tracker,
        failover_ref,
    } = llm_stack;
    let Toolbox {
        tools,
        image_config,
        video_config,
        speech_config,
        ..
    } = toolbox;
    let Stores {
        memory_store,
        training_store,
        citation_store,
        leader_gate,
        self_learning_store,
        deep_expertise_store,
        sessions,
    } = stores;
    let Admin {
        identity_store_handle,
        is_admin,
    } = admin;
    let Vaults {
        agent_registry,
        known_vaults,
    } = vaults;
    let Overrides {
        active_model,
        active_max_tokens,
        active_max_rounds,
        active_compaction,
        active_routing,
    } = overrides;
    let Skills {
        skill_state,
        telemetry_store,
        cost_tracker,
        permission_evaluator,
        ..
    } = skills;
    let scheduler = scheduler.map(|scheduler| scheduler as Arc<dyn crate::hosts::SchedulerHost>);
    let JobRunning {
        active_runners: active_runners_for_jobs,
        capability_gate,
        job_runner,
    } = jobs;
    let Media {
        vision_tiers,
        describer,
        transcription_tiers,
        transcriber,
    } = media;

    info!(agent = %agent_id, "Agent service ready");
    let live_turns = crate::live_turns::LiveTurns::new(agent_id.as_str());
    let service = AgentService {
        config: shared_config,
        config_path,
        agent_id,
        identity: Arc::clone(&identity),
        sessions,
        llm: Arc::from(llm),
        tools: tools.clone(),
        policy,
        scheduler,
        job_runner,
        health_tracker,
        heartbeat_handle: parking_lot::RwLock::new(None),
        check_signer,
        nats: parking_lot::RwLock::new(None),
        leader_gate: Arc::clone(&leader_gate),
        image_broadcast: parking_lot::RwLock::new(None),
        surface_registry,
        slack_bot_identities: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        workspace,
        skills_dir: config.skills.resolved_skills_dir(&config.hub.data_dir),
        skill_state,
        active_model,
        vault: vault.clone(),
        global_vault: global_vault.clone(),
        token_pool: Some(pool),
        failover_provider: failover_ref,
        provider_kind: Arc::clone(&provider_kind),
        tier_key: Arc::clone(&tier_key),
        admission,
        watches: Default::default(),
        image_config: Arc::clone(&image_config),
        video_config: Arc::clone(&video_config),
        speech_config: Arc::clone(&speech_config),
        effective_llm,
        original_default_model,
        current_default_model: Arc::clone(&current_default_model),
        active_compaction,
        active_max_tokens,
        active_max_rounds,
        lock_manager,
        cost_tracker,
        cost_governor: Arc::new(crate::hub_core::cost_governor::CostGovernor::new(
            Arc::clone(&telemetry_store),
            config.budget.clone(),
            config.user_timezone(),
        )),
        telemetry_store: telemetry_store.clone(),
        active_routing,
        active_runners: active_runners_for_jobs,
        live_turns,
        background_work: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        user_map: agent_config.map(|a| a.user_map.clone()).unwrap_or_default(),
        identity_store: identity_store_handle,
        team_store: team_store_handle,
        remote_mcp_tokens: Arc::new(parking_lot::RwLock::new(None)),
        permission_writer: Arc::new(parking_lot::RwLock::new(None)),
        permission_evaluator,
        permission_gate: Arc::new(parking_lot::RwLock::new(None)),
        capability_gate,
        approval_router: Arc::new(parking_lot::RwLock::new(None)),
        pending_approval_store: Arc::new(parking_lot::RwLock::new(None)),
        maintenance_runner: crate::hub_core::slot::Slot::new(),
        tool_access: Arc::new(parking_lot::RwLock::new(None)),
        mcp_manager: Arc::new(parking_lot::RwLock::new(None)),
        access_gate: parking_lot::RwLock::new(None),
        scope_gate: parking_lot::RwLock::new(None),
        used_tools: parking_lot::Mutex::new(None),
        compile_gate,
        tool_usage_refresh: std::sync::OnceLock::new(),
        turn_rig: crate::hub_core::slot::Slot::new(),
        hosts: crate::serve::ServiceHosts::new(
            Arc::new(crate::turn_loop::TurnLoop),
            Arc::new(crate::reconfigure::Reconfigurer),
        ),
        known_vaults,
        coding_session_reload: std::sync::OnceLock::new(),
        local_agents: if let Some(shared) = shared_local_agents {
            Some(shared)
        } else if config.local_agents.enabled {
            Some(Arc::new(
                crate::hub_core::local_agent_registry::LocalAgentRegistry::new(),
            ))
        } else {
            None
        },
        knowledge_service: knowledge_service_arc,
        workflow_runtime: std::sync::OnceLock::new(),
        playbook_runtime: std::sync::OnceLock::new(),
        consolidation_runtime: std::sync::OnceLock::new(),
        monitor_engine: std::sync::OnceLock::new(),
        monitor_event_tx: std::sync::OnceLock::new(),
        playbook_cards: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        config_equivalences: config_equivalences_cell,
        memory_store: Some(Arc::clone(&memory_store)),
        training_store: training_store.clone(),
        effective_training: config.training.clone(),
        citation_store: citation_store.clone(),
        self_learning_store: parking_lot::RwLock::new(self_learning_store),
        deep_expertise_store: parking_lot::RwLock::new(deep_expertise_store),
        self_learning_handle: parking_lot::RwLock::new(None),
        resolved,
        resolved_at_start: Arc::clone(&resolved_at_build),
        config_rejected: parking_lot::RwLock::new(None),
        config_applied: parking_lot::RwLock::new(
            config_text
                .as_deref()
                .map(workforce_orchestrator::workflow_store::content_hash),
        ),
        agent_registry,
        trace_writer,
        owner_override,
        name_override: parking_lot::RwLock::new(None),
        logins,
        providers: Arc::clone(&providers) as Arc<dyn crate::hosts::ProviderHost>,
        subscriptions,
        vision_tiers,
        describer,
        transcription_tiers,
        transcriber,
    };

    // Register tools that need Arc<AgentService> - must happen after construction
    let service_arc = Arc::new(service);
    // Definition writes (workflow/playbook/monitor tools) share one rule
    // with the gateway; the tools apply it to the turn's user through the
    // admin gate above and the late-bound team store.
    let def_write_auth = crate::def_write_auth::TurnDefWriteAuth::default()
        .with_admin_policy(Arc::clone(&is_admin))
        .with_team_store(Arc::clone(&service_arc.team_store));
    register_service_tools(&service_arc, &config, &is_admin).await;

    let workflow_runtime = match shared_workflow_runtime {
        Some(runtime) => runtime,
        None => build_workflow_runtime(&service_arc, &config).await?,
    };
    service_arc.register_tool(Arc::new(
        crate::workflow_tool::WorkflowTool::new(
            Arc::clone(&workflow_runtime),
            service_arc.agent_id.to_string(),
            service_arc.scheduler.clone(),
        )
        .with_write_auth(def_write_auth.clone()),
    ));
    if let Some(store) = crate::store::global_document_store() {
        crate::pipeline_tool::register_hub_reads(
            &service_arc.tools,
            store,
            Arc::clone(&is_admin),
            Arc::clone(&workflow_runtime) as Arc<dyn crate::pipeline::HubReads>,
        );
    }
    let _ = service_arc.workflow_runtime.set(workflow_runtime);

    let playbook_runtime = crate::playbook_runtime::runtime(&config);
    service_arc.register_tool(Arc::new(
        crate::playbook_tool::PlaybookTool::new(Arc::clone(&playbook_runtime))
            .with_write_auth(def_write_auth.clone()),
    ));
    let _ = service_arc.playbook_runtime.set(playbook_runtime.clone());
    crate::playbook_runtime::wire(&service_arc, &config, playbook_runtime).await;

    crate::consolidation_runtime::wire(&service_arc, &config).await;

    crate::monitor::wire(
        &service_arc,
        &config,
        agent_config,
        &def_write_auth,
        &adapter_loader,
    )
    .await;

    super::background::start_sweeps(&service_arc);

    memory_metrics.spawn();

    Ok(service_arc)
}

/// Workflow runtime + its coding handles + hot-reload watcher. Exactly one
/// runtime exists per hub: a second would load the same definitions,
/// register a duplicate of every schedule trigger against the shared
/// scheduler, and execute the fired job as its own agent.
async fn build_workflow_runtime(
    service: &Arc<AgentService>,
    config: &WorkforceConfig,
) -> Result<Arc<dyn crate::hosts::WorkflowHost>> {
    use std::time::Duration;

    let data_dir = std::path::PathBuf::from(&config.hub.data_dir);
    let workflows_dir = data_dir.join("workflows");

    let worktree_manager = Arc::new(
        crate::code::worktree::WorktreeManager::new_with_worktree_root(
            &data_dir,
            config.hub.worktree_root_path(),
        )?,
    );
    if let Some(hot) = crate::hub_core::layout::WorkRoots::from_hub(&config.hub).hot {
        crate::code::worktree::ensure_shared_dir(&hot)?;
    }
    crate::code::utils::init_worktree_manager(Arc::clone(&worktree_manager));

    // Clean up stale worktrees from previous runs.
    // Preserve worktrees for running AND paused workflows — paused
    // workflows are awaiting human approval and will resume.
    // The GC's preserve-set comes from the telemetry store. A SQLite
    // telemetry store is per-instance, so in a clustered deployment it
    // cannot see peers' active workflows and the GC would delete their
    // live worktrees — the spawned tasks refuse to sweep in that
    // combination. Checked at run time, not here: the election flag is
    // late-bound after this build.
    let telemetry_is_shared = config.telemetry.backend == "postgres";

    {
        let ts = Arc::clone(&service.telemetry_store);
        let wt_mgr = Arc::clone(&worktree_manager);
        let gc_gate = Arc::clone(&service.leader_gate);
        tokio::spawn(async move {
            if !gc_gate.wait_boot_singleton().await {
                return;
            }
            if gc_gate.is_clustered() && !telemetry_is_shared {
                warn!(
                    "Worktree GC disabled: clustered deployment with per-instance \
                 SQLite telemetry cannot see peers' active workflows. Set \
                 [telemetry] backend = \"postgres\" to re-enable."
                );
                return;
            }
            // Computed after the boot wait so workflows started in the
            // meantime are in the preserve-set.
            let active_ids: std::collections::HashSet<String> = ts
                .list_workflow_instances(None, None)
                .unwrap_or_default()
                .iter()
                .filter(|i| {
                    i.status != "completed"
                        && i.status != "failed"
                        && i.status != "exited"
                        && i.status != "cancelled"
                })
                .map(|i| i.id.clone())
                .collect();
            if let Err(e) = wt_mgr
                .cleanup_stale(
                    &active_ids,
                    Duration::from_secs(86400),
                    Duration::from_secs(3600),
                )
                .await
            {
                warn!(error = %e, "Failed to clean up stale worktrees on startup");
            }
        });
    }

    // Periodic cleanup: query active workflow IDs each tick so paused
    // workflows are always preserved.
    {
        let ts = Arc::clone(&service.telemetry_store);
        let active_ids_fn = Arc::new(move || -> std::collections::HashSet<String> {
            ts.list_workflow_instances(None, None)
                .unwrap_or_default()
                .iter()
                .filter(|i| {
                    i.status != "completed"
                        && i.status != "failed"
                        && i.status != "exited"
                        && i.status != "cancelled"
                })
                .map(|i| i.id.clone())
                .collect()
        });
        worktree_manager.spawn_cleanup_task(
            Duration::from_secs(3600),
            Duration::from_secs(86400),
            Duration::from_secs(3600), // 1hr grace period for terminal workflows
            active_ids_fn,
            Arc::clone(&service.leader_gate),
            telemetry_is_shared,
        );
    }

    // Periodic GC for session workspaces: `cc-sessions/{uuid}/` (a
    // `/cc` thread's clones) and `work/session/{scope}/`
    // (coding.session rounds, open_workspace, benches — ungoverned
    // until now, and the tree that filled the Harbor volume that
    // also holds the vault). Active sessions refresh a `.lastseen`
    // sentinel; anything idle past its TTL is reclaimed.
    //
    // Spawned once per PROCESS, not once per agent: this builder runs
    // for each configured agent while a sweep pass walks every agent's
    // tree, so an unguarded spawn gives N sweepers racing each other's
    // teardown — duplicated full-tree walks, and failures logged for
    // reclaims that actually succeeded. The sweepers then consult the
    // leader gate on every tick, which keeps a clustered fleet sharing
    // one data volume down to a single sweeper and follows a change of
    // leader instead of sampling it at boot.
    static SESSION_SWEEPERS_STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if SESSION_SWEEPERS_STARTED.set(()).is_ok() {
        let interval = Duration::from_secs(3600);
        crate::projects::cc_session::spawn_sweeper(
            data_dir.clone(),
            interval,
            Duration::from_secs(config.projects.cc_session_ttl_secs),
            Arc::clone(&service.leader_gate),
        );
        crate::projects::work_session::spawn_sweeper(
            crate::hub_core::layout::WorkRoots::from_hub(&config.hub),
            interval,
            Duration::from_secs(config.projects.work_session_ttl_secs),
            Arc::clone(&service.leader_gate),
        );
    }

    // MCP servers are now spawned via `McpServerManager` (see
    // `crate::mcp_servers::manager`). The orchestrator
    // (cli/main.rs after `AgentService::build_for_agent`)
    // constructs one manager per agent, calls
    // `hydrate_from_store()` to re-attach persisted servers,
    // and `import_from_toml()` to seed any new
    // `[mcp.servers.*]` config entries. This replaces the
    // legacy inline `spawn_all` call which couldn't support
    // restart-free agent-driven install.

    let workflow_runtime = Arc::new(crate::workflow_runtime::WorkflowRuntime::new(
        Arc::clone(service),
        Arc::clone(&service.telemetry_store),
        workflows_dir.clone(),
    ));
    workflow_runtime.set_worktree_manager(Arc::clone(&worktree_manager));
    // Workflow `claude-code` steps run on the coding worker when the
    // install's executor is remote, serving the `[mcp]` tool surface
    // workflow steps get.
    match crate::coding_sandbox::remote_launcher_for(
        &config.coding_session.executor,
        &config.exec,
        crate::coding_sandbox::McpSurface {
            exposed_tools: config.mcp.step_patterns(),
        },
    ) {
        Ok(Some(launcher)) => workflow_runtime.set_remote_launcher(launcher),
        Ok(None) => {}
        Err(e) => warn!(error = %e, "workflow claude-code steps stay local"),
    }
    workflow_runtime.set_coding_engines(
        Arc::new(crate::coding_engine::Engines::from_config(
            &config.coding_session,
            &data_dir,
        )),
        config.coding_session.engine.clone(),
    );

    Ok(crate::workflow_runtime::wire(service, config, workflow_runtime).await)
}

/// Register the advisor tool for this service. Idempotent — refuses
/// duplicate registration. Called by `AgentRegistry::from_config`
/// once the registry weak ref is in place so the `source = "agent"`
/// path has cross-agent lookup. The tool is interactive-only by
/// construction: it reads `CURRENT_SESSION_ID` / falls back to the
/// most recent session, neither of which is set in workflow steps.
pub fn register_advisor_tool(service: &Arc<AgentService>) {
    if !service.effective_advisor().enabled {
        return;
    }
    if service.tools.has(crate::advisor::AdvisorTool::NAME) {
        return;
    }
    let tool = crate::advisor::AdvisorTool::new(
        Arc::clone(service),
        service.effective_advisor().clone(),
        Arc::clone(&service.agent_registry),
    );
    service.tools.register(Arc::new(tool));
    info!(
        target: "advisor",
        agent = %service.agent_id,
        source = ?service.effective_advisor().source,
        "advisor tool registered"
    );
}

/// Wire the agent-job executor into this service's job runner so the
/// `subagent` tool can run isolated sub-agents as `Agent` jobs. Idempotent.
/// Needs the service `Arc`, so it is called after construction (see
/// [`crate::agent_registry::AgentRegistry::wire_advisor_tools_for`]).
pub fn wire_agent_jobs(service: &Arc<AgentService>) {
    if let Some(runner) = &service.job_runner {
        runner.set_agent_executor(Arc::new(ServiceAgentExecutor {
            service: Arc::clone(service),
        }));
    }
}

/// Register the agent-driver Claude Code tool (`coding.session`) for this
/// service. Idempotent. A chain that runs only on subscription logins is
/// offered when the agent's `[llm.logins]` has somewhere to hold one,
/// like `coding.claude`. The project store
/// and base data dir are passed in because they aren't `AgentService`
/// fields; everything else (sessions, vault) comes from `service`.
pub fn register_coding_session_tool(
    service: &Arc<AgentService>,
    project_store: Arc<dyn workforce_projects::ProjectStore>,
    roots: crate::hub_core::layout::WorkRoots,
    mcp_enabled: bool,
    coding: workforce_common::settings::CodingSessionConfig,
    exec: workforce_common::settings::ExecConfig,
) {
    // An engine chain that runs without a login is always offered; one
    // that runs only on logins, while the agent offers a kind of them.
    // Each round resolves its own payer's login, or tells them how to
    // connect.
    let offered = !coding.engine.subscription_only()
        || coding
            .engine
            .kinds()
            .iter()
            .filter_map(|kind| engine_login(*kind))
            .any(|kind| offers_logins_of(service, kind));
    if !offered {
        if ["coding.session", "coding.fanout"]
            .map(|tool| service.tools.unregister(tool))
            .contains(&true)
        {
            info!(target: "coding_session", agent = %service.agent_id, "coding.session + coding.fanout withdrawn");
        }
        return;
    }
    if service.tools.has("coding.session") {
        return;
    }
    // `coding.fanout` shares the same workspace + tool surface, gated on the
    // same Claude Code token, so it's registered alongside `coding.session`.
    let profile = crate::coding_session_tool::DriverProfile::from(&coding);
    let launcher = match crate::coding_sandbox::hub_launcher(
        roots.clone(),
        &coding,
        &exec,
        crate::coding_sandbox::McpSurface {
            exposed_tools: mcp_enabled.then(|| coding.effective_exposed_tools()),
        },
    ) {
        Ok(launcher) => launcher,
        Err(e) => {
            tracing::error!(
                target: "coding_session",
                agent = %service.agent_id,
                error = %e,
                "coding.session not registered: the coding executor cannot be built"
            );
            return;
        }
    };
    let engines = crate::coding_engine::Engines::from_config(&coding, &roots.data_dir);
    let engine = match engines.configured(&coding.engine, None, None) {
        Ok(engine) => engine,
        Err(e) => {
            tracing::error!(
                target: "coding_session",
                agent = %service.agent_id,
                error = %e,
                "coding.session not registered: the coding engine chain cannot be built"
            );
            return;
        }
    };
    let fanout = Arc::new(crate::coding_fanout_tool::CodingFanoutTool::new(
        Arc::clone(service),
        Arc::clone(&project_store),
        profile.clone(),
        Arc::clone(&launcher),
        Arc::clone(&engine),
    ));
    fanout.host();
    service.tools.register(fanout);
    let tool = crate::coding_session_tool::CodingSessionTool::new(
        Arc::clone(service),
        project_store,
        Arc::clone(&service.sessions),
        roots.data_dir,
        service.agent_id.as_str().to_string(),
        service.vault.clone(),
        service.global_vault.clone(),
        profile,
        launcher,
        engine,
    );
    service.tools.register(Arc::new(tool));
    info!(
        target: "coding_session",
        agent = %service.agent_id,
        "coding.session + coding.fanout tools registered"
    );
}

/// Offer the ad-hoc Claude Code tool (`coding.claude`) while the agent
/// [offers Claude logins](AgentService::offers_logins_of), and withdraw it once
/// it no longer does.
pub fn register_claude_code_tool(service: &Arc<AgentService>) {
    let offered = offers_logins_of(
        service,
        workforce_common::subscription::SubscriptionKind::Claude,
    );
    match (offered, service.tools.has("coding.claude")) {
        (true, false) => {}
        (false, true) => {
            service.tools.unregister("coding.claude");
            info!(agent = %service.agent_id, "Claude Code tool withdrawn (coding.claude)");
            return;
        }
        _ => return,
    }
    let paths = vec![
        service.workspace.clone(),
        crate::code::worktree::worktrees_dir(
            std::path::Path::new(&service.config.hub.data_dir),
            service.config.hub.worktree_root_path(),
        ),
    ];
    service
        .tools
        .register(Arc::new(crate::claude_code_tool::ClaudeCodeTool::new(
            Arc::clone(service),
            paths,
        )));
    info!(agent = %service.agent_id, "Claude Code tool registered (coding.claude)");
}

/// Register the always-on `chatgpt.login` tool: a ChatGPT sign-in from
/// chat, its code shown to the person alone.
pub fn register_chatgpt_login_tool(service: &Arc<AgentService>) {
    if service.tools.has("chatgpt.login") {
        return;
    }
    let tool = crate::chatgpt_login_tool::ChatGptLoginTool::new(
        service.subscriptions().device_logins().clone(),
        Arc::clone(&service.logins),
        Arc::clone(&service.sessions),
        crate::authz::make_admin_policy(Arc::clone(&service.identity_store)),
    );
    service.tools.register(Arc::new(tool));
}

/// Register the always-on `codex.setup_auth` tool, the Codex twin of
/// `claude_code.setup_token`.
pub fn register_codex_setup_auth_tool(
    service: &Arc<AgentService>,
    reload_coding_session: crate::hub_core::coding_sessions::ReloadCodingSession,
) {
    if service.tools.has("codex.setup_auth") {
        return;
    }
    let Some(vault) = service.vault.clone() else {
        return;
    };
    let tool = crate::codex_setup_auth_tool::CodexSetupAuthTool::new(
        vault,
        Arc::clone(&service.logins),
        crate::authz::make_admin_policy(Arc::clone(&service.identity_store)),
        Arc::clone(&service.known_vaults),
        reload_coding_session,
    );
    service.tools.register(Arc::new(tool));
    info!(
        target: "coding_session",
        agent = %service.agent_id,
        "codex.setup_auth tool registered"
    );
}

/// Register the always-on `claude_code.setup_token` tool — the
/// agent-callable installer for a user's Claude plan OAuth token. Wired
/// independently of `coding.session` so an agent without a token
/// configured can still expose a tool to *install* one.
pub fn register_claude_code_setup_token_tool(
    service: &Arc<AgentService>,
    reload_coding_session: crate::hub_core::coding_sessions::ReloadCodingSession,
) {
    if service.tools.has("claude_code.setup_token") {
        return;
    }
    let Some(vault) = service.vault.clone() else {
        // No agent vault → nowhere to store the token. The shipping
        // hub always has one; skip silently for the test/dev case.
        return;
    };
    let tool = crate::claude_code_setup_token_tool::ClaudeCodeSetupTokenTool::new(
        vault,
        reload_coding_session,
        service.agent_id.as_str().to_string(),
    );
    service.tools.register(Arc::new(tool));
    info!(
        target: "coding_session",
        agent = %service.agent_id,
        "claude_code.setup_token tool registered"
    );
}

/// Adapter: exposes the agent's `ToolRegistry` via the `ToolExecutor` trait
/// so the job runner can dispatch tool calls without a circular dependency.
pub(super) struct RegistryToolExecutor {
    pub(super) registry: Arc<workforce_agent::tools::registry::ToolRegistry>,
    pub(super) agent_id: String,
    /// The service's late-bound admin gate, which a job's tool call meets as
    /// the call would in the turn that submitted it.
    pub(super) capability_gate:
        Arc<parking_lot::RwLock<Option<Arc<dyn workforce_agent::runner::CapabilityGate>>>>,
}

impl RegistryToolExecutor {
    /// Why `user` may not make this call. A job naming nobody is a system
    /// job, which the gate leaves alone as it does a system turn.
    fn denied(
        &self,
        user: Option<&str>,
        tool_name: &str,
        params: &serde_json::Value,
    ) -> Option<String> {
        let gate = self.capability_gate.read().clone()?;
        gate.deny_reason(user?, &self.agent_id, tool_name, params)
    }
}

#[async_trait::async_trait]
impl crate::job_runner::ToolExecutor for RegistryToolExecutor {
    async fn execute_tool(
        &self,
        tool_name: &str,
        params: serde_json::Value,
    ) -> eyre::Result<workforce_agent::tools::ToolResult> {
        let user = crate::user_config::current_identity_id();
        if let Some(refusal) = self.refusal(user.as_deref(), tool_name, &params) {
            return Ok(workforce_agent::tools::ToolResult::failure(refusal, 0));
        }
        let tool = self
            .registry
            .get(tool_name)
            .ok_or_else(|| eyre::eyre!("Tool '{}' not found in registry", tool_name))?;
        tool.execute(params).await
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        self.registry.get(tool_name).is_some()
    }

    fn refusal(
        &self,
        user: Option<&str>,
        tool_name: &str,
        params: &serde_json::Value,
    ) -> Option<String> {
        workforce_agent::tools::dispatch::unattended_refusal(&self.registry, tool_name)
            .or_else(|| self.denied(user, tool_name, params))
    }
}

/// Adapter: delivers job-completion notifications to channel surfaces (Slack
/// etc.) via the shared `SurfaceRegistry`. Fires only when a job sets a
/// `notify_channel`, so silent jobs stay silent.
pub(super) struct SurfaceNotificationSink {
    pub(super) surface_registry: Arc<tokio::sync::RwLock<SurfaceRegistry>>,
}

#[async_trait::async_trait]
impl crate::job_runner::NotificationSink for SurfaceNotificationSink {
    async fn send_notification(
        &self,
        channel: &str,
        thread: Option<&str>,
        text: &str,
    ) -> eyre::Result<()> {
        let message = workforce_common::channels::OutboundMessage {
            channel: channel.to_string(),
            text: text.to_string(),
            thread_id: thread.map(|t| t.to_string()),
            username: None,
            icon_url: None,
            blocks: None,
        };
        let registry = self.surface_registry.read().await;
        for surface in registry.all() {
            if let Err(e) = surface.send_message(&message).await {
                warn!(platform = %surface.platform(), error = %e, "Failed to send job notification");
            }
        }
        Ok(())
    }
}

/// Adapter: runs an isolated sub-agent for an agent job via [`AgentService::run_isolated`].
pub(super) struct ServiceAgentExecutor {
    pub(super) service: Arc<AgentService>,
}

#[async_trait::async_trait]
impl crate::job_runner::AgentExecutor for ServiceAgentExecutor {
    async fn run_agent_job(
        &self,
        config: &crate::hub_core::jobs::AgentJobConfig,
        channel: &str,
    ) -> eyre::Result<crate::hub_core::jobs::AgentRunResult> {
        crate::scheduler::descending_from(
            config.automated_origin.clone(),
            self.run_as_submitted(config, channel),
        )
        .await
    }
}

impl ServiceAgentExecutor {
    /// An agent job, as the identity and automation that submitted it.
    async fn run_as_submitted(
        &self,
        config: &crate::hub_core::jobs::AgentJobConfig,
        channel: &str,
    ) -> eyre::Result<crate::hub_core::jobs::AgentRunResult> {
        let principal = config.principal();
        let isolated = IsolatedRun::from(principal.clone());
        let run = |message: String| {
            let isolated = &isolated;
            async move {
                self.service
                    .run_isolated(
                        channel,
                        &message,
                        config.model.as_deref(),
                        config.system_prompt.as_deref(),
                        isolated,
                        config.profile,
                        None,
                        None,
                    )
                    .await
            }
        };
        let Some(spec) = &config.handoff else {
            return run(config.task.clone()).await;
        };
        let policy = self.service.handoff();
        let verifier =
            if crate::handoff::wants_verifier(&policy, &spec.outputs, spec.verify.as_ref()) {
                Some(
                    crate::handoff::handoff_verifier(
                        &self.service,
                        &policy,
                        workforce_common::subscription::Origin::Subagent,
                        principal.billing().map(str::to_string),
                    )
                    .await,
                )
            } else {
                None
            };
        // The verifier's calls are the job's, so they run as whoever it runs
        // as; the detached task this runs in inherits no identity.
        crate::user_config::as_principal(
            &principal,
            crate::handoff::hand_over(
                spec,
                &config.task,
                &policy,
                verifier
                    .as_ref()
                    .map(|v| v as &dyn crate::handoff::Verifier),
                run,
            ),
        )
        .await
    }
}

/// Records job execution metrics into the telemetry store for stats reporting.
struct JobStatsRecorder {
    telemetry_store: Arc<dyn TelemetryStore>,
    agent_id: String,
}

#[async_trait::async_trait]
impl crate::job_runner::StatsRecorder for JobStatsRecorder {
    async fn record_job_telemetry(
        &self,
        manifest: &crate::hub_core::jobs::JobManifest,
        _output: Option<&crate::hub_core::jobs::JobOutput>,
    ) {
        let task_type = format!("job:{}", manifest.job_type);
        let duration_ms = (manifest.elapsed_secs() * 1000.0) as u64;
        let started_at = manifest
            .started_at
            .unwrap_or(manifest.submitted_at)
            .to_rfc3339();
        let completed_at = manifest
            .completed_at
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc3339();

        let detail = match &manifest.config {
            crate::hub_core::jobs::JobConfig::Tool(t) => t.tool.clone(),
            crate::hub_core::jobs::JobConfig::Shell(s) => truncate_with_ellipsis(&s.command, 100),
            crate::hub_core::jobs::JobConfig::Monitor(m) => format!("monitor:{}", m.tool),
            crate::hub_core::jobs::JobConfig::Agent(a) => {
                truncate_with_ellipsis(&format!("agent:{}", a.task), 100)
            }
        };

        let job_description = match &manifest.config {
            crate::hub_core::jobs::JobConfig::Tool(t) => format!("tool:{}", t.tool),
            crate::hub_core::jobs::JobConfig::Shell(s) => {
                truncate_with_ellipsis(&format!("shell:{}", s.command), 200)
            }
            crate::hub_core::jobs::JobConfig::Monitor(m) => format!("monitor:{}", m.tool),
            crate::hub_core::jobs::JobConfig::Agent(a) => {
                truncate_with_ellipsis(&format!("agent:{}", a.task), 200)
            }
        };

        let task = TaskTelemetryRecord {
            agent_id: self.agent_id.clone(),
            model: String::new(), // Jobs don't use LLM models directly
            provider: "job_runner".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_usd: 0.0, // Jobs have no direct LLM cost
            rounds: 1,
            duration_ms,
            tool_invocation_count: 1,
            success: manifest.status == crate::hub_core::jobs::JobStatus::Completed,
            error_type: manifest.error.as_ref().map(|e| {
                if e.contains("timed out") {
                    "timeout".to_string()
                } else if e.contains("cancelled") {
                    "cancelled".to_string()
                } else {
                    "job_error".to_string()
                }
            }),
            retry_count: 0,
            task_type: Some(task_type),
            task_id: Some(manifest.job_id.clone()),
            started_at,
            completed_at,
            forecast_cost_usd: None,
            forecast_rounds: None,
            forecast_confidence: None,
            team_id: None,
            session_id: None,
            model_switches: None,
            ticket_id: None,
            identity_id: crate::user_config::current_billing_identity_id(),
            description: Some(job_description),
        };

        // Record a single tool telemetry entry for the job execution.
        let tool_record = ToolTelemetryRecord {
            round_number: 0,
            tool_name: detail,
            success: manifest.status == crate::hub_core::jobs::JobStatus::Completed,
            duration_ms,
            policy_decision: "allow".to_string(),
            input_size_bytes: 0,
            output_size_bytes: 0,
            denial_reason: None,
        };

        if let Err(e) = self
            .telemetry_store
            .record_task_telemetry(&task, &[], &[tool_record])
        {
            tracing::warn!(
                job_id = %manifest.job_id,
                error = %e,
                "Failed to record job telemetry"
            );
        }
    }
}

/// Injects job completion notifications into active agent sessions.
struct JobSessionInjector {
    active_runners: Arc<tokio::sync::RwLock<HashMap<String, ActiveRunnerHandle>>>,
}

#[async_trait::async_trait]
impl crate::job_runner::SessionInjector for JobSessionInjector {
    async fn inject_job_completion(
        &self,
        session_channel: &str,
        message: &str,
    ) -> eyre::Result<bool> {
        let runners = self.active_runners.read().await;
        if let Some(handle) = runners.get(session_channel) {
            let msg = InjectedMessage::UserMessage {
                text: format!("[Job Result] {}", message),
                user: "system".to_string(),
                external_id: None,
            };
            // try_send: never block under the runners read guard — a full or
            // closed channel means "not delivered", and the caller falls back.
            Ok(handle.inject_tx.try_send(msg).is_ok())
        } else {
            Ok(false)
        }
    }
}

async fn register_service_tools(
    service: &Arc<AgentService>,
    config: &WorkforceConfig,
    is_admin: &crate::authz::IsAdminFn,
) {
    service.register_tool(Arc::new(crate::compaction_tool::CompactionTool::new(
        Arc::clone(service),
    )));
    service.register_tool(Arc::new(crate::config_tool::ConfigTool::new(
        Arc::clone(service),
        shared_workflow_store(config).await,
    )));
    crate::environment_tools::register_environment_tools(service);
    service.register_tool(Arc::new(
        crate::budget_tool::BudgetTool::new(Arc::clone(service))
            .with_admin_policy(Arc::clone(is_admin)),
    ));
    service.register_tool(Arc::new(crate::investigate::InvestigateTool::new(
        Arc::clone(service),
    )));
    service.register_tool(Arc::new(crate::research_report::ResearchReportTool::new(
        Arc::clone(service),
        service.workspace.clone(),
    )));
    service.register_tool(Arc::new(
        crate::subagent_tool::SubAgentTool::new(Arc::clone(service))
            .with_admin_policy(Arc::clone(is_admin)),
    ));
    service.register_tool(Arc::new(crate::watch::WatchTool::new(Arc::clone(service))));
    service.register_tool(Arc::new(crate::context_tool::ContextStatusTool::new(
        Arc::clone(service),
    )));
    service.register_tool(Arc::new(crate::context_tool::ContextSuggestTool::new(
        Arc::clone(service),
        service.workspace.clone(),
    )));
    service.register_tool(Arc::new(crate::context_tool::ContextDropTool::new(
        Arc::clone(service),
    )));
    service.register_tool(Arc::new(
        crate::effective_settings::tool::SettingsTool::new(Arc::clone(service))
            .with_admin_policy(Arc::clone(is_admin)),
    ));
    service.register_tool(Arc::new(crate::version_tool::SystemVersionTool));
}

/// What `build_inner` resolves before it builds anything: the agent, its
/// workspace, and its configuration as the three layers leave it.
struct Setup {
    agent_id: AgentId,
    workspace: PathBuf,
    identity: Arc<RwLock<AgentIdentity>>,
    workspace_config: Option<workforce_common::settings::AgentWorkspaceConfig>,
    config_text: Option<String>,
    effective_llm: LlmConfig,
    effective_memory: workforce_common::settings::MemoryConfig,
    config_equivalences: Option<HashMap<String, Vec<String>>>,
    config_equivalences_cell: workforce_agent::llm::tiers::EquivalencesCell,
    provider_kind_val: ProviderKind,
    provider_kind: Arc<parking_lot::RwLock<ProviderKind>>,
    tier_key_val: String,
    tier_key: Arc<parking_lot::RwLock<String>>,
    shared_config: Arc<WorkforceConfig>,
    admission: crate::workspace_admission::WorkspaceAdmission,
    resolved: crate::hub_core::resolved_config::ConfigCell,
    resolved_at_build: Arc<crate::hub_core::resolved_config::ResolvedConfig>,
    original_default_model: Arc<parking_lot::RwLock<String>>,
    model: String,
    current_default_model: Arc<parking_lot::RwLock<String>>,
}

/// The agent's vaults and who pays for its calls.
struct Credentials {
    vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
    team_store_handle: crate::def_write_auth::TeamStoreHandle,
    owner_override: crate::subscription::OwnerOverride,
    logins: Arc<crate::subscription::AgentLogins>,
    system_plans: Option<crate::subscription::PlanLogins>,
    providers: Arc<crate::agent_llm::AgentProviders>,
    subscriptions: crate::hub_core::subscriptions::SharedSubscriptions,
}

/// The pool, its failover wrapper, and the provider the service calls.
struct LlmStack {
    pool: Arc<PooledProvider>,
    llm: Box<dyn workforce_agent::llm::provider::LlmProvider>,
    health_tracker: Option<Arc<HealthTracker>>,
    failover_ref: Option<Arc<FailoverProvider>>,
}

/// The tool registry, the paths its tools may touch, and the media config
/// cells the generation tools are rebuilt from.
struct Toolbox {
    runner: Option<workforce_agent::tools::exec_runner::RunnerEndpoint>,
    tool_paths: Vec<PathBuf>,
    worktrees_dir: PathBuf,
    tools: Arc<workforce_agent::tools::registry::ToolRegistry>,
    image_config: Arc<parking_lot::RwLock<Option<LlmImageGenerationConfig>>>,
    video_config: Arc<parking_lot::RwLock<Option<LlmVideoGenerationConfig>>>,
    speech_config: Arc<parking_lot::RwLock<Option<LlmSpeechConfig>>>,
    speech_home: PathBuf,
    reload_image_tool: workforce_agent::tools::image_gen::ReloadImageTool,
}

/// The stores opened at build (memory, training, citations, self-learning,
/// sessions) and the leader gate their background work consults.
struct Stores {
    memory_store: Arc<dyn workforce_memory::store::MemoryStore>,
    training_store: Option<Arc<dyn workforce_training::TrainingStore>>,
    citation_store: crate::training::SessionCitationStore,
    leader_gate: Arc<crate::hub_core::leader_gate::LeaderGate>,
    self_learning_store: Option<Arc<dyn workforce_self_learning::SelfLearningStore>>,
    deep_expertise_store: Option<Arc<dyn workforce_self_learning::DeepExpertiseStore>>,
    sessions: Arc<dyn SessionStore>,
}

/// The late-bound identity store and the admin gate every privileged tool
/// shares.
struct Admin {
    identity_store_handle:
        Arc<parking_lot::RwLock<Option<Arc<dyn workforce_identity::store::IdentityStore>>>>,
    is_admin: crate::authz::IsAdminFn,
}

/// The registry handle and every vault a login may sit in.
struct Vaults {
    agent_registry: crate::hosts::RegistryCell,
    known_vaults: crate::subscription::store::KnownVaults,
}

/// The runtime overrides, as the vault last left them.
struct Overrides {
    active_model: Arc<parking_lot::RwLock<Option<String>>>,
    active_max_tokens: Arc<parking_lot::RwLock<Option<u32>>>,
    active_max_rounds: Arc<parking_lot::RwLock<Option<u32>>>,
    active_compaction:
        Arc<parking_lot::RwLock<Option<workforce_common::session::CompactionConfig>>>,
    active_routing: Arc<parking_lot::RwLock<Option<workforce_agent::model_router::RoutingConfig>>>,
}

/// The skill surface, and the telemetry and policy cells the system tools
/// read per call.
struct Skills {
    skill_state: Arc<crate::hub_core::skill_state::SkillStateTracker>,
    wasm_invoker_slot: workforce_wasm_runtime::types::SkillInvokerSlot,
    telemetry_store: Arc<dyn TelemetryStore>,
    cost_tracker: Arc<CostTracker>,
    permission_evaluator:
        Arc<parking_lot::RwLock<Option<Arc<workforce_permissions::cache::PermissionEvaluator>>>>,
    policy_cell: Arc<std::sync::OnceLock<Arc<PolicyEngine>>>,
}

/// The job runner and the runner map it injects completions into.
struct JobRunning {
    active_runners: Arc<tokio::sync::RwLock<HashMap<String, ActiveRunnerHandle>>>,
    capability_gate:
        Arc<parking_lot::RwLock<Option<Arc<dyn workforce_agent::runner::CapabilityGate>>>>,
    job_runner: Option<Arc<dyn crate::hosts::JobHost>>,
}

/// The vision and transcription providers, built once so their caches span
/// turns.
struct Media {
    vision_tiers: Arc<workforce_agent::llm::vision_tiers::VisionTierRegistry>,
    describer: Describer,
    transcription_tiers: Arc<workforce_agent::llm::transcription_tiers::TranscriptionTierRegistry>,
    transcriber: Transcriber,
}

impl Setup {
    async fn resolve(
        config: &WorkforceConfig,
        agent_config: Option<&workforce_common::settings::AgentConfig>,
    ) -> Result<Self> {
        let agent_id = agent_config
            .map(|a| AgentId::new(&a.id))
            .unwrap_or_else(|| AgentId::new("default"));

        let workspace = crate::hub_core::layout::agent_workspace(
            std::path::Path::new(&config.hub.data_dir),
            agent_id.as_str(),
            agent_config.and_then(|a| a.workspace.as_deref()),
        );
        // Create the agent workspace as a shared, group-writable+setgid tree so
        // the dispatched agent runner (git/cargo) can write the files the hub
        // created here, and vice versa (audit F7). setgid makes descendants
        // inherit the `workforce` group without a recursive chown.
        crate::code::worktree::ensure_shared_dir(&workspace)?;
        AgentIdentity::bootstrap_defaults(&workspace)?;
        crate::private_state::close_agent_state(&workspace);
        let workspace = workspace.canonicalize()?;

        let user_tz = config.user_timezone();
        let mut identity = AgentIdentity::load_with_timezone(&workspace, user_tz)?;
        identity.max_daily_notes_chars = config.max_daily_notes_chars;
        let identity = Arc::new(RwLock::new(identity));

        info!(agent = %agent_id, workspace = %workspace.display(), "Loading agent");

        // Load per-agent workspace config ({workspace}/config.toml)
        // This is the second layer in the three-layer config system:
        //   1. workforce.toml (CI-managed global defaults)
        //   2. {workspace}/config.toml (per-agent runtime config, agent-managed)
        //   3. vault (secrets only — API keys, tokens)
        let (workspace_config, config_text) =
            workforce_common::settings::AgentWorkspaceConfig::load_with_text(&workspace)?.unzip();
        if workspace_config.is_some() {
            info!(agent = %agent_id, "Loaded workspace config from {}/config.toml", workspace.display());
        }

        // Set temporal config on identity (defaults to enabled if no [temporal] section)
        {
            let temporal_config = workspace_config
                .as_ref()
                .and_then(|ws| ws.temporal.clone())
                .or_else(|| Some(workforce_common::temporal::TemporalConfig::default()));
            if temporal_config.as_ref().is_some_and(|c| c.enabled) {
                info!(agent = %agent_id, "Temporal perception layer enabled");
            }
            let mut id = identity.write().await;
            id.set_temporal_config(temporal_config);
        }

        let mut effective_llm =
            merged_effective_llm(agent_config, workspace_config.as_ref(), &config.llm);

        // Extract model tier equivalences for provider-agnostic routing. The
        // local drives construction-time tier resolution; the cell is the live
        // handle shared with the pool and re-read on every hot-reload.
        let config_equivalences = workspace_config
            .as_ref()
            .and_then(|ws| ws.llm.as_ref())
            .and_then(|llm| llm.equivalences.clone());
        let config_equivalences_cell: workforce_agent::llm::tiers::EquivalencesCell = Arc::new(
            parking_lot::RwLock::new(config_equivalences.clone().map(Arc::new)),
        );

        let (provider_kind_val, tier_key_val) = agent_provider(&effective_llm.provider)
            .map_err(|e| eyre::eyre!("agent {agent_id}: {e}"))?;
        let provider_kind = Arc::new(parking_lot::RwLock::new(provider_kind_val));
        let tier_key = Arc::new(parking_lot::RwLock::new(tier_key_val.clone()));
        let shared_config = Arc::new(config.clone());
        let admission = crate::workspace_admission::WorkspaceAdmission::new(
            Arc::clone(&shared_config),
            agent_id.as_str(),
            Arc::clone(&tier_key),
        );

        // Resolve the hot-reloadable runtime config (memory / compaction /
        // advisor) once the tier is known, then keep owned snapshots for the
        // rest of construction. Re-derived without a restart by `reload_config`.
        let resolved = crate::hub_core::resolved_config::ConfigCell::new(
            crate::hub_core::resolved_config::ResolvedConfig::resolve(
                agent_config,
                workspace_config.as_ref(),
                config,
                &tier_key_val,
                config_equivalences.as_ref(),
            ),
        );
        let resolved_at_build = resolved.snapshot();
        crate::subscription::validate_agent_llm(
            &effective_llm,
            &resolved_at_build,
            &crate::subscription::Install::of(config),
        )
        .map_err(|e| eyre::eyre!("agent {agent_id}: {e}"))?;
        // Subscription `claude -p` rounds go through the privacy gateway when
        // `[privacy]` says so. Install-wide, so it is set once here; an
        // incomplete route refuses to boot rather than send rounds direct.
        let privacy_route = crate::claude_code::PrivacyRoute::from_config(&config.privacy)?;
        match &privacy_route {
            Some(route) => {
                tracing::info!(gateway = %route.gateway_url, "privacy gateway routing enabled for subscription rounds")
            }
            None => tracing::debug!("privacy gateway routing off"),
        }
        crate::claude_code::set_privacy_route(privacy_route);
        // Decision backends (Kev / Jev) for the workflow branch fallback.
        // Install-wide, set once; an inconsistent `[decision]` refuses to boot.
        crate::decide::install_from_config(&config.decision)?;
        crate::decision_store::install(config).await;
        let effective_memory = resolved_at_build.memory.clone();

        // Capture pre-resolution spec before the block below overwrites
        // default_model with its concrete tier resolution.
        let original_default_model = Arc::new(parking_lot::RwLock::new(
            effective_llm.default_model.clone(),
        ));
        {
            use workforce_agent::llm::tiers::TierRegistry;
            let tiers = TierRegistry::new(config_equivalences.as_ref());
            let resolved = tiers.resolve_model(&effective_llm.default_model, &tier_key_val);
            if resolved != effective_llm.default_model {
                info!(
                    original = %effective_llm.default_model,
                    resolved = %resolved,
                    "Resolved tier name in default_model"
                );
                effective_llm.default_model = resolved;
            }
            // Background-task model tiers (passive / consolidation / compaction)
            // are resolved inside `ResolvedConfig::resolve`; only the LLM
            // default_model and failover chain are resolved here.
            let failover = resolved_failover(&effective_llm.failover, &tiers, &tier_key_val);
            if failover != effective_llm.failover {
                info!(
                    original = ?effective_llm.failover,
                    resolved = ?failover,
                    "Resolved tier names in failover chain"
                );
                effective_llm.failover = failover;
            }
        }

        let model = effective_llm.default_model.clone();
        let current_default_model = Arc::new(parking_lot::RwLock::new(
            effective_llm.default_model.clone(),
        ));
        Ok(Self {
            agent_id,
            workspace,
            identity,
            workspace_config,
            config_text,
            effective_llm,
            effective_memory,
            config_equivalences,
            config_equivalences_cell,
            provider_kind_val,
            provider_kind,
            tier_key_val,
            tier_key,
            shared_config,
            admission,
            resolved,
            resolved_at_build,
            original_default_model,
            model,
            current_default_model,
        })
    }
}

impl Credentials {
    async fn open(
        config: &WorkforceConfig,
        setup: &Setup,
        agent_config: Option<&workforce_common::settings::AgentConfig>,
        global_vault: Option<Arc<Vault>>,
        subscriptions: crate::subscription::Subscriptions,
    ) -> Result<Self> {
        let Setup {
            agent_id,
            workspace,
            resolved,
            ..
        } = setup;
        // Open per-agent vault early so it can be used for API key resolution.
        let vault = crate::vault::open_agent(
            config,
            agent_id.as_str(),
            agent_config.and_then(|a| a.vault.as_ref()),
            workspace,
        )
        .await?;

        // Bound once the identity store exists (`set_team_store`); handed to the
        // service and to every tool context that decides who a team reaches.
        let team_store_handle: crate::def_write_auth::TeamStoreHandle =
            Arc::new(parking_lot::RwLock::new(None));
        let owner_override: crate::subscription::OwnerOverride =
            Arc::new(parking_lot::RwLock::new(None));
        let logins = Arc::new(crate::subscription::AgentLogins::new(
            agent_id.as_str(),
            crate::subscription::Vaults {
                agent: vault.as_ref(),
                global: global_vault.as_ref(),
            },
            config
                .agent_by_id(agent_id.as_str())
                .map(|a| (a.owner_id.clone(), a.owner_type.clone()))
                .unwrap_or_default(),
            Arc::clone(&owner_override),
            Arc::clone(&team_store_handle),
            resolved.clone(),
        ));
        let shared = subscriptions.shared().clone();
        let system_plans = global_vault.clone().map(|vault| {
            crate::subscription::PlanLogins::new(
                subscriptions.clone(),
                crate::subscription::CredentialScope::system(vault, &config.subscription),
            )
        });
        let providers = Arc::new(crate::agent_llm::AgentProviders::new(
            vault.clone(),
            global_vault.clone(),
            crate::subscription::PlanLogins::new(
                subscriptions,
                crate::subscription::CredentialScope::Agent(Arc::clone(&logins)),
            ),
        ));
        Ok(Self {
            vault,
            global_vault,
            team_store_handle,
            owner_override,
            logins,
            system_plans,
            providers,
            subscriptions: shared,
        })
    }
}

impl LlmStack {
    async fn build(setup: &Setup, credentials: &Credentials) -> Result<Self> {
        let Setup {
            agent_id,
            effective_llm,
            config_equivalences,
            config_equivalences_cell,
            provider_kind_val,
            tier_key,
            resolved_at_build,
            original_default_model,
            model,
            ..
        } = setup;
        let provider_kind_val = *provider_kind_val;
        let Credentials { providers, .. } = credentials;
        // Build token pool provider (supports multi-token failover). The
        // pool is the final chokepoint before a model id leaves for the
        // wire — wire the tier resolver in so any tier name that slipped
        // past upstream defences self-corrects here too.
        let pool = Arc::new(
            PooledProvider::new()
                .with_tier_resolver(workforce_agent::llm::pool::TierResolver {
                    tier_key: Arc::clone(tier_key),
                    config_equivalences: Arc::clone(config_equivalences_cell),
                })
                .with_pacing(workforce_agent::llm::pool::PacingPolicy::from_config(
                    &effective_llm.pacing,
                )),
        );
        // Liveness watchdog windows — the pool enforces them on every
        // install, with or without a failover chain.
        pool.set_llm_timeouts(
            effective_llm
                .retry
                .as_ref()
                .map(workforce_agent::llm::failover::LlmTimeouts::from)
                .unwrap_or_default(),
        );
        {
            let default_provider = providers
                .provider(
                    provider_kind_val,
                    Some(model),
                    effective_llm.thinking_budget_tokens,
                    effective_llm.zai_subscription,
                    effective_llm.api_key.clone(),
                )
                .await
                .map_err(|e| eyre::eyre!("agent {agent_id}: {e}"))?;
            pool.add("default", default_provider).await;
            info!("Default LLM provider added to pool");
        }

        let base_llm: Box<dyn workforce_agent::llm::provider::LlmProvider> =
            Box::new(Arc::clone(&pool));

        // Load additional accounts from config into pool
        let pooled_accounts: &[String] = if provider_kind_val.is_subscription() {
            if !effective_llm.accounts.is_empty() {
                warn!(
                    provider = provider_kind_val.canonical_name(),
                    accounts = ?effective_llm.accounts,
                    "[llm] accounts ignored: {}",
                    crate::subscription::LOGINS_ARE_NOT_ACCOUNTS
                );
            }
            &[]
        } else {
            &effective_llm.accounts
        };
        for env_name in pooled_accounts {
            if let Ok(token) = std::env::var(env_name) {
                match create_named_provider_async(
                    provider_kind_val,
                    env_name,
                    &token,
                    Some(model),
                    effective_llm.thinking_budget_tokens,
                    effective_llm.zai_subscription,
                )
                .await
                {
                    Ok(provider) => {
                        pool.add(env_name, Arc::from(provider)).await;
                        info!(
                            name = env_name,
                            "Additional account loaded from env into pool"
                        );
                    }
                    Err(e) => {
                        warn!(
                            name = env_name,
                            error = %e,
                            "Failed to load additional account from env"
                        );
                    }
                }
            } else {
                warn!(
                    env_var = env_name,
                    "Additional account env var not set, skipping"
                );
            }
        }

        // Wrap with a failover provider when a chain or aliases are
        // configured, or when a spec routes to a subscription provider: that
        // route must reach its own provider, never the metered primary.
        let external_routes = crate::agent_llm::external_routes(
            provider_kind_val,
            crate::subscription::chain_specs(effective_llm, resolved_at_build),
        );
        #[allow(clippy::type_complexity)]
        let (llm, health_tracker, failover_ref): (
            Box<dyn workforce_agent::llm::provider::LlmProvider>,
            Option<Arc<HealthTracker>>,
            Option<Arc<FailoverProvider>>,
        ) = if effective_llm.has_failover_config()
            || external_routes.values().any(|kind| kind.is_subscription())
        {
            let failover_config = FailoverConfig {
                failover: effective_llm.failover.clone(),
                aliases: effective_llm.aliases.clone(),
                retry: effective_llm
                    .retry
                    .as_ref()
                    .map(to_failover_retry)
                    .unwrap_or_default(),
                health: effective_llm
                    .health
                    .as_ref()
                    .map(|h| workforce_agent::llm::failover::HealthConfig {
                        failure_threshold: h.failure_threshold,
                        window_secs: h.window_secs,
                        recovery_secs: h.recovery_secs,
                    })
                    .unwrap_or_default(),
            };
            info!(
                failover = ?failover_config.failover,
                aliases = failover_config.aliases.len(),
                "Model failover enabled"
            );
            let tier_reg =
                workforce_agent::llm::tiers::TierRegistry::new(config_equivalences.as_ref());
            let base: Arc<dyn workforce_agent::llm::provider::LlmProvider> = Arc::from(base_llm);
            crate::artifact_index::offer_agent_tagger(provider_kind_val, Arc::clone(&base));
            let provider = Arc::new(
                FailoverProvider::new(base, failover_config)
                    .with_tier_registry(tier_reg)
                    .with_hub_default_model(original_default_model.read().clone()),
            );
            providers
                .register_externals(&provider, external_routes)
                .await;

            let tracker = Arc::clone(provider.health_tracker());
            (
                Box::new(Arc::clone(&provider)),
                Some(tracker),
                Some(provider),
            )
        } else {
            (base_llm, None, None)
        };
        Ok(Self {
            pool,
            llm,
            health_tracker,
            failover_ref,
        })
    }
}

impl Toolbox {
    fn new(config: &WorkforceConfig, setup: &Setup, credentials: &Credentials) -> Self {
        let Setup {
            agent_id,
            workspace,
            ..
        } = setup;
        let Credentials {
            vault,
            global_vault,
            ..
        } = credentials;
        let runner = workforce_agent::tools::exec_runner::RunnerEndpoint::from_config(&config.exec);
        crate::coding_sandbox::record_worker_protocols();
        let tool_secrets = ToolSecrets {
            brave_api_key: Some(crate::credentials::brave_source(
                vault.clone(),
                global_vault.clone(),
            )),
            // Dispatch exec-class commands to the privilege-separated runner
            // daemon when a socket is configured (audit F7).
            runner: runner.clone(),
        };

        // Build tools (wrapped in Arc early so system tools can share the reference)
        // Include worktrees dir so filesystem/exec tools can access workflow worktree files.
        let mut tool_paths = vec![workspace.clone()];
        let worktrees_dir = crate::code::worktree::worktrees_dir(
            std::path::Path::new(&config.hub.data_dir),
            config.hub.worktree_root_path(),
        );
        tool_paths.push(worktrees_dir.clone());
        tool_paths.extend(crate::hub_core::layout::WorkRoots::from_hub(&config.hub).hot);
        let tools = Arc::new(default_tools(tool_paths.clone(), tool_secrets));

        // Shared image-config handle and rebuild closure. Created early so the
        // vault tools (`vault.set`/`vault.delete`) and the LLM tools
        // (`system.add_account`/`system.remove_account`) — all registered
        // before `install_image_tool` runs — can carry the same callback.
        // `install_image_tool` then populates the lock with its recorded
        // config; the closure reads whichever config is current at invocation
        // and rebuilds the image provider against the live vault.
        let image_config: Arc<parking_lot::RwLock<Option<LlmImageGenerationConfig>>> =
            Arc::new(parking_lot::RwLock::new(None));
        let video_config: Arc<parking_lot::RwLock<Option<LlmVideoGenerationConfig>>> =
            Arc::new(parking_lot::RwLock::new(None));
        let speech_config: Arc<parking_lot::RwLock<Option<LlmSpeechConfig>>> =
            Arc::new(parking_lot::RwLock::new(None));
        // Default download dir for the builtin (Piper) speech backend.
        let speech_home = workspace.join("piper");
        // One shared media-reload callback rebuilds BOTH the image and video
        // tools from the live vault — they share the credential namespace
        // (`llm/accounts/gemini`), so a single hook keeps the wiring (vault
        // tools, LlmToolContext) untouched while covering the new video tool.
        let reload_image_tool: workforce_agent::tools::image_gen::ReloadImageTool = {
            let tools = Arc::clone(&tools);
            let agent_id = agent_id.clone();
            let image_config = Arc::clone(&image_config);
            let video_config = Arc::clone(&video_config);
            let speech_config_for_reload = Arc::clone(&speech_config);
            let speech_home_for_reload = speech_home.clone();
            let vault_for_reload = vault.clone();
            let global_for_reload = global_vault.clone();
            Arc::new(move || {
                let img = workforce_agent::tools::image_gen::reinstall_image_tool(
                    &tools,
                    agent_id.as_str(),
                    image_config.read().clone().as_ref(),
                    crate::credentials::image_keys(
                        image_config.read().as_ref(),
                        &vault_for_reload,
                        &global_for_reload,
                    ),
                );
                img.log(agent_id.as_str());
                let vid = workforce_agent::tools::video_gen::reinstall_video_tool(
                    &tools,
                    agent_id.as_str(),
                    video_config.read().clone().as_ref(),
                    crate::credentials::video_keys(
                        video_config.read().as_ref(),
                        &vault_for_reload,
                        &global_for_reload,
                    ),
                );
                vid.log(agent_id.as_str());
                let sp = workforce_agent::tools::speech_gen::reinstall_speech_tool(
                    &tools,
                    agent_id.as_str(),
                    speech_config_for_reload.read().clone().as_ref(),
                    &speech_home_for_reload,
                    crate::credentials::speech_keys(
                        speech_config_for_reload.read().as_ref(),
                        &vault_for_reload,
                        &global_for_reload,
                    ),
                    Some(Arc::new(crate::hub_core::formatting::normalize_for_tts)),
                    Some(crate::vault::speech_user_voice_resolver(
                        vault_for_reload.clone(),
                        global_for_reload.clone(),
                    )),
                );
                sp.log(agent_id.as_str());
            })
        };
        Self {
            runner,
            tool_paths,
            worktrees_dir,
            tools,
            image_config,
            video_config,
            speech_config,
            speech_home,
            reload_image_tool,
        }
    }
}

impl Stores {
    async fn open(config: &WorkforceConfig, setup: &Setup) -> Result<Self> {
        let Setup {
            agent_id,
            workspace,
            resolved_at_build,
            ..
        } = setup;
        // Memory store: shared fail-closed resolver (honors [memory] backend,
        // retries a configured Weaviate, never silently downgrades).
        let memory_store = crate::state::resolve_memory_store(config).await?;

        let training_store: Option<Arc<dyn workforce_training::TrainingStore>> =
            crate::training::open_training_store(config).await?;
        let citation_store = crate::training::SessionCitationStore::new();

        // Created before the background spawns below so each singleton can
        // consult it per tick; the election flag is late-bound in main.rs.
        let leader_gate = Arc::new(crate::hub_core::leader_gate::LeaderGate::default());

        if let Some(ref training) = training_store {
            if config.training.cleanup.enabled {
                crate::training::spawn_sweep(
                    Arc::clone(training),
                    Arc::clone(&memory_store),
                    crate::training::SweepOptions {
                        verify_only: config.training.cleanup.verify_only,
                    },
                    std::time::Duration::from_secs(config.training.cleanup.timeout_secs),
                    Arc::clone(&leader_gate),
                );
            }
        }

        // Open the self-learning store eagerly only when the resolved (per-agent
        // → global) self_learning is enabled; otherwise leave None and open it
        // lazily on a hot-enable (see `ensure_self_learning_store`). One concrete
        // store, two trait-object views (atomic items + deep-expertise tree).
        let curation_stores: Option<crate::self_learning::backend::CurationStores> =
            if resolved_at_build.self_learning.enabled {
                crate::self_learning::backend::open_effective(
                    &resolved_at_build.self_learning,
                    config,
                )
                .await?
            } else {
                None
            };
        let self_learning_store = curation_stores.as_ref().map(|s| s.self_learning.clone());
        let deep_expertise_store = curation_stores.map(|s| s.deep_expertise);

        // Session store — JSONL-backed (default) or the shared Postgres
        // backend when `[sessions] backend = "postgres"`. Built here (before
        // the memory/notes tools) so those tools share this exact store
        // rather than constructing their own JSONL store and bypassing the
        // configured backend. The PG backend is one shared pool, scoped per
        // agent by `agent_id`.
        let sessions: Arc<dyn SessionStore> =
            crate::session_store_factory::build_session_store(config, workspace, agent_id.as_str())
                .await?;
        Ok(Self {
            memory_store,
            training_store,
            citation_store,
            leader_gate,
            self_learning_store,
            deep_expertise_store,
            sessions,
        })
    }
}

fn register_memory_tools(
    config: &WorkforceConfig,
    setup: &Setup,
    stores: &Stores,
    toolbox: &Toolbox,
    agent_config: Option<&workforce_common::settings::AgentConfig>,
) {
    let Setup {
        agent_id,
        workspace,
        workspace_config,
        effective_memory,
        admission,
        resolved,
        ..
    } = setup;
    let Stores {
        memory_store,
        sessions,
        ..
    } = stores;
    let Toolbox { tools, .. } = toolbox;
    // Register memory tool (consolidated — replaces memory_search + memory)
    let decay_rate = workspace_config
        .as_ref()
        .and_then(|ws| ws.temporal.as_ref())
        .map_or(0.005, |t| t.decay_rate);
    let mut memory_tool = MemoryTool::new(
        Arc::clone(memory_store),
        agent_id.clone(),
        workspace.clone(),
    )
    .with_sessions(Arc::clone(sessions))
    .with_decay_rate(decay_rate)
    .with_min_certainty(effective_memory.recall_min_certainty);
    if let Some(ac) = agent_config {
        if ac.owner_type == workforce_common::settings::OwnerType::Team {
            if let Some(ref tid) = ac.owner_id {
                memory_tool = memory_tool.with_team_id(tid);
            }
        }
    }
    tools.register(Arc::new(memory_tool));
    tools.register(Arc::new(crate::preferences::PreferenceTool::new(
        Arc::clone(memory_store),
        agent_id.clone(),
    )));
    tools.register(Arc::new(
        NotesTool::with_store(
            workspace.clone(),
            Arc::clone(memory_store),
            agent_id.clone(),
        )
        .with_sessions(Arc::clone(sessions))
        // Live cell, not a snapshot: a hot-reloaded `agent_recall_scope`
        // must reach `notes.read_daily` without a service rebuild, same
        // as the per-turn injection paths.
        .with_live_config(resolved.clone()),
    ));
    // Generic data → .xlsx export, delivered to the channel as a file.
    tools.register(Arc::new(
        workforce_agent::tools::export_xlsx::ExportXlsxTool,
    ));
    // Generic data → streamed .csv export (no workbook in memory; for large
    // downloads), delivered to the channel as a file.
    tools.register(Arc::new(workforce_agent::tools::export_csv::ExportCsvTool));
    // Structured spec → typeset PDF report / PNG infographic, delivered as a
    // file. The workspace
    // path lets it read live `[report]` brand themes per render.
    tools.register(Arc::new(
        workforce_agent::tools::report::GenerateReportTool::new(workspace.clone()),
    ));
    tools.register(Arc::new(
        workforce_agent::tools::report::SetReportThemeTool::new(
            workspace.clone(),
            Arc::new(admission.clone()),
        ),
    ));
    // Same spec → editable native document (.docx/.pptx); a native Google
    // Doc/Slides follows by uploading the artifact with drive convert_to.
    tools.register(Arc::new(
        workforce_agent::tools::report::ExportReportTool::new(workspace.clone()),
    ));
    // Structure → a diagram delivered as a file (PNG for chat, SVG, or an
    // editable .excalidraw). The same `wf-diagram` spec the customer-facing
    // widget renders, held to it by a golden corpus
    //.
    tools.register(Arc::new(workforce_agent::tools::diagram::DiagramTool));
    // Quantities → a chart delivered as an image (PNG for chat and email, or
    // SVG). The same `wf-chart` spec the customer-facing widget renders,
    // held to it by a golden corpus.
    tools.register(Arc::new(workforce_agent::tools::chart::ChartTool::new(
        workspace.clone(),
    )));
    // Data plane: read-only SQL over staged tabular artifacts + shape
    // inspection.
    tools.register(Arc::new(workforce_agent::tools::data::DataQueryTool));
    tools.register(Arc::new(workforce_agent::tools::data::DataInspectTool));
    info!(
        "Memory tools registered (vector store: {})",
        config.weaviate.enabled
    );
}

fn register_code_tools(
    config: &WorkforceConfig,
    setup: &Setup,
    toolbox: &Toolbox,
) -> Arc<dyn crate::hosts::CompileGateHost> {
    let Setup {
        agent_id,
        workspace,
        resolved,
        ..
    } = setup;
    let Toolbox {
        runner,
        tool_paths,
        worktrees_dir,
        tools,
        ..
    } = toolbox;
    // Register native coding tools
    // Include worktrees dir so code tools can access workflow worktree files.
    let mut allowed_paths = vec![workspace.clone()];
    allowed_paths.push(worktrees_dir.clone());
    tools.register(Arc::new(crate::code::CodeReadTool::new(
        allowed_paths.clone(),
    )));
    tools.register(Arc::new(crate::code::CodeEditTool::new(
        allowed_paths.clone(),
    )));
    tools.register(Arc::new(crate::code::CodeSearchTool::new(
        allowed_paths.clone(),
    )));
    // Code tools that spawn git/cargo route their child processes through
    // the privilege-separated runner when a socket is configured, fail-closed
    // if unreachable (audit F7). Same value already resolved for
    // `ExecCommand`/`ToolSecrets` above; `None` keeps the in-process dev
    // behaviour. `code.tree`/`code.diff` route too — they run `git
    // status`/`git diff`, which honour agent-planted fsmonitor/external-diff
    // drivers, so they must run as the agent in agent-writable trees.
    let code_runner = runner.clone();
    tools.register(Arc::new(
        crate::code::CodeTreeTool::new(allowed_paths.clone()).with_runner(code_runner.clone()),
    ));
    tools.register(Arc::new(crate::code::CodeSymbolsTool::new(
        allowed_paths.clone(),
    )));
    tools.register(Arc::new(
        crate::code::CodeDiffTool::new(allowed_paths.clone()).with_runner(code_runner.clone()),
    ));
    tools.register(Arc::new(
        crate::code::CodeTestTool::new(allowed_paths.clone()).with_runner(code_runner.clone()),
    ));
    tools.register(Arc::new(
        crate::code::CodeLintTool::new(allowed_paths.clone()).with_runner(code_runner.clone()),
    ));
    tools.register(Arc::new(
        crate::code::CodeBuildTool::new(allowed_paths.clone()).with_runner(code_runner.clone()),
    ));
    tools.register(Arc::new(
        crate::code::CodeGitTool::new(allowed_paths.clone()).with_runner(code_runner.clone()),
    ));
    tools.register(Arc::new(crate::code::CodeFindReplaceTool::new(
        allowed_paths.clone(),
    )));
    tools.register(Arc::new(crate::code::CodeFindTool::new(
        allowed_paths.clone(),
    )));
    tools.register(Arc::new(
        crate::code::CodeCargoTool::new(allowed_paths.clone()).with_runner(code_runner.clone()),
    ));
    // `code.refactor`: registered unless the install turns it off;
    // whether this agent has it is read per call from its own config,
    // and a turn it is off for does not offer it.
    if config.tools.refactor.mode != workforce_common::settings::ProgramsMode::Off {
        let install = config.tools.refactor.clone();
        let cell = resolved.clone();
        tools.register(Arc::new(crate::code::CodeRefactorTool::new(
            allowed_paths.clone(),
            Arc::new(move || install.for_agent(cell.snapshot().refactor.as_ref())),
            &config.tools.refactor,
            code_runner.clone(),
        )));
    }
    info!("Native coding tools registered");
    crate::code::compile_gate::GateCore::new(
        compile_gate_roots(config, agent_id.as_str(), tool_paths, runner.as_ref()),
        Arc::new(crate::code::compile_gate::RunnerExecutor(
            code_runner.clone(),
        )),
    )
}

fn register_github_tools(setup: &Setup, credentials: &Credentials, toolbox: &Toolbox) {
    let Setup { workspace, .. } = setup;
    let Credentials {
        vault,
        global_vault,
        ..
    } = credentials;
    let Toolbox { tools, .. } = toolbox;
    // Register GitHub local repo tools (vault-authenticated clone & list)
    if let Some(ref vault) = vault {
        for tool in
            crate::github_local::github_local_tools(crate::github_local::GithubLocalContext {
                vault: Arc::clone(vault),
                global_vault: global_vault.clone(),
                workspace: workspace.clone(),
            })
        {
            tools.register(tool);
        }
    }
}

async fn register_aws_tools(config: &WorkforceConfig, toolbox: &Toolbox) {
    let Toolbox { tools, .. } = toolbox;
    if config.aws_s3.is_ready() {
        let s3_cfg = config.aws_s3.clone();
        let role_arn = s3_cfg
            .assume_role_arn
            .clone()
            .expect("is_ready() guarantees assume_role_arn is Some");
        let dropped = crate::aws::require_approval_entries_outside_allowlist(&s3_cfg);
        if !dropped.is_empty() {
            warn!(
                dropped = ?dropped,
                bucket_allowlist = ?s3_cfg.bucket_allowlist,
                "aws.s3 require_approval_buckets entries outside bucket_allowlist were dropped from constitutional policy; check for typos",
            );
        }
        match crate::aws::AwsS3ClientFactory::load(role_arn).await {
            Ok(factory) => {
                let backend: Arc<dyn crate::aws::S3Backend> =
                    Arc::new(crate::aws::SdkS3Backend::new(factory));
                let allowlist =
                    Arc::new(crate::aws::Allowlist::new(s3_cfg.bucket_allowlist.clone()));
                tools.register(Arc::new(crate::aws::AwsS3ListTool::new(
                    Arc::clone(&backend),
                    Arc::clone(&allowlist),
                )));
                tools.register(Arc::new(crate::aws::AwsS3HeadTool::new(
                    Arc::clone(&backend),
                    Arc::clone(&allowlist),
                )));
                tools.register(Arc::new(crate::aws::AwsS3GetTool::new(
                    Arc::clone(&backend),
                    Arc::clone(&allowlist),
                    s3_cfg.max_get_bytes,
                )));
                tools.register(Arc::new(crate::aws::AwsS3CollectTool::new(
                    backend,
                    allowlist,
                    s3_cfg.max_get_bytes,
                )));
                info!(
                    buckets = ?s3_cfg.bucket_allowlist,
                    require_approval = ?s3_cfg.require_approval_buckets,
                    max_get_bytes = s3_cfg.max_get_bytes,
                    "AWS S3 tools registered (aws.s3.list / aws.s3.head / aws.s3.get / aws.s3.collect)",
                );
            }
            Err(err) => warn!(
                error = %err,
                "aws.s3 enabled but SDK config load failed; tools not registered",
            ),
        }
    } else if config.aws_s3.enabled {
        let missing: Vec<&'static str> = [
            config
                .aws_s3
                .bucket_allowlist
                .is_empty()
                .then_some("bucket_allowlist"),
            config
                .aws_s3
                .assume_role_arn
                .is_none()
                .then_some("assume_role_arn"),
        ]
        .into_iter()
        .flatten()
        .collect();
        warn!(
            missing = ?missing,
            "aws.s3.enabled=true but prerequisites are missing; tools not registered. Set [aws_s3].bucket_allowlist and [aws_s3].assume_role_arn (or WORKFORCE_AWS_S3_BUCKET_ALLOWLIST / WORKFORCE_AWS_S3_ASSUME_ROLE_ARN).",
        );
    }

    if config.aws_infra.is_ready() {
        let infra_cfg = config.aws_infra.clone();
        let role_arn = infra_cfg
            .assume_role_arn
            .clone()
            .expect("is_ready() guarantees assume_role_arn is Some");
        match crate::aws::SdkConfigFactory::load(role_arn, infra_cfg.region.clone()).await {
            Ok(factory) => {
                let backend: Arc<dyn crate::aws::InfraBackend> =
                    Arc::new(crate::aws::SdkInfraBackend::new(Arc::new(factory)));
                tools.register(Arc::new(crate::aws::AwsEcsListClustersTool::new(
                    Arc::clone(&backend),
                )));
                tools.register(Arc::new(crate::aws::AwsEcsDescribeServicesTool::new(
                    Arc::clone(&backend),
                )));
                tools.register(Arc::new(crate::aws::AwsEcsListServicesTool::new(
                    Arc::clone(&backend),
                )));
                tools.register(Arc::new(crate::aws::AwsEcsListTasksTool::new(Arc::clone(
                    &backend,
                ))));
                tools.register(Arc::new(crate::aws::AwsEcsDescribeTasksTool::new(
                    Arc::clone(&backend),
                )));
                tools.register(Arc::new(crate::aws::AwsEcsDescribeTaskDefinitionTool::new(
                    Arc::clone(&backend),
                )));
                tools.register(Arc::new(
                    crate::aws::AwsEcsDescribeContainerInstancesTool::new(Arc::clone(&backend)),
                ));
                tools.register(Arc::new(crate::aws::AwsEcrListRepositoriesTool::new(
                    Arc::clone(&backend),
                )));
                tools.register(Arc::new(crate::aws::AwsEcrDescribeImagesTool::new(
                    Arc::clone(&backend),
                )));
                tools.register(Arc::new(crate::aws::AwsElbDescribeTargetHealthTool::new(
                    Arc::clone(&backend),
                )));
                tools.register(Arc::new(crate::aws::AwsRdsDescribeDbClustersTool::new(
                    Arc::clone(&backend),
                )));
                tools.register(Arc::new(crate::aws::AwsRdsDescribeEventsTool::new(
                    Arc::clone(&backend),
                )));
                tools.register(Arc::new(crate::aws::AwsStsGetCallerIdentityTool::new(
                    Arc::clone(&backend),
                )));
                tools.register(Arc::new(crate::aws::AwsLogsDescribeLogGroupsTool::new(
                    Arc::clone(&backend),
                )));
                tools.register(Arc::new(crate::aws::AwsLogsFilterEventsTool::new(
                    Arc::clone(&backend),
                )));
                tools.register(Arc::new(crate::aws::AwsCloudwatchGetMetricDataTool::new(
                    Arc::clone(&backend),
                )));
                tools.register(Arc::new(crate::aws::AwsDeployStatusTool::new(backend)));
                info!(
                    region = ?infra_cfg.region,
                    role_arn = %infra_cfg.assume_role_arn.as_deref().unwrap_or_default(),
                    tools = ?crate::aws::infra::TOOL_NAMES,
                    "AWS infra tools registered",
                );
            }
            Err(err) => warn!(
                error = %err,
                "aws.infra enabled but SDK config load failed; tools not registered",
            ),
        }
    } else if config.aws_infra.enabled {
        warn!(
            "aws_infra.enabled=true but assume_role_arn is missing; tools not registered. Set [aws_infra].assume_role_arn (or WORKFORCE_AWS_INFRA_ASSUME_ROLE_ARN).",
        );
    }
}

fn register_knowledge(
    config: &WorkforceConfig,
    setup: &Setup,
    credentials: &Credentials,
    toolbox: &Toolbox,
    stores: &Stores,
    shared_knowledge: Option<Arc<crate::knowledge::KnowledgeService>>,
    start_root_indexing: bool,
) -> Option<Arc<crate::knowledge::KnowledgeService>> {
    let Setup {
        agent_id,
        workspace,
        resolved,
        ..
    } = setup;
    let Credentials {
        global_vault,
        system_plans,
        ..
    } = credentials;
    let Toolbox { tools, .. } = toolbox;
    let Stores { memory_store, .. } = stores;
    // Register knowledge tools (codebase understanding).
    //
    // When `shared_knowledge` is provided (multi-agent path), we reuse it
    // and skip FS-watcher / startup-indexing — the caller (AgentRegistry)
    // manages those once for all agents.
    let (knowledge_service_arc, owns_knowledge) = if let Some(ks) = shared_knowledge {
        crate::knowledge::register_tools(tools, &ks, workspace);
        info!("Knowledge tools registered (shared)");
        (Some(ks), false)
    } else {
        // Single-agent path — create our own knowledge service
        match crate::knowledge::create_service(
            &config.knowledge,
            crate::knowledge::VectorBackend::of(config),
            &workspace.to_string_lossy(),
            &config.codegraph,
            config.postgres.as_ref(),
            crate::knowledge::HubSummariserBuilder::new(config.knowledge.summariser.clone())
                .with_global_vault(global_vault.clone())
                .with_plans(system_plans.clone()),
            crate::knowledge::VectorUpkeep::Kept,
        ) {
            Some(ks) => {
                crate::knowledge::register_tools(tools, &ks, workspace);
                info!("Knowledge tools registered");
                (Some(ks), true)
            }
            None => (None, false),
        }
    };

    if let Some(ref ks) = knowledge_service_arc {
        tools.register(Arc::new(crate::context_fuse_tool::ContextFuseTool::new(
            Arc::clone(ks),
            Arc::clone(memory_store),
            workspace.clone(),
            agent_id.clone(),
            resolved.clone(),
        )));
        info!("Context fuse tool registered (context.fuse)");
    }

    // Start FS watcher and startup indexing only if we own the service
    // (i.e. single-agent path). In multi-agent mode, the AgentRegistry
    // starts these once for the shared instance.
    // Root-of-service indexing (startup index + FS watcher) is for the
    // single-repo model. In the hub's canonical model the MaintenanceRunner
    // is the sole indexer, so `cli::main` passes `start_root_indexing =
    // false` to avoid a redundant, never-pulled duplicate index of the root.
    if owns_knowledge && start_root_indexing {
        if let Some(ref ks) = knowledge_service_arc {
            crate::knowledge::start_background(ks, config);
        }
    }
    knowledge_service_arc
}

impl Admin {
    fn new() -> Self {
        // Identity-store handle is late-bound (set after agent boots via
        // `set_identity_store`). Built here — before the scheduler/reminder and
        // vault tool families below — so a single shared admin gate threads into
        // every privileged tool. The handle is also wired into the whoami tool
        // (registered later).
        let identity_store_handle: Arc<
            parking_lot::RwLock<Option<Arc<dyn workforce_identity::store::IdentityStore>>>,
        > = Arc::new(parking_lot::RwLock::new(None));

        // The single status-aware admin gate (deny-by-default). Built once here
        // and threaded into every privileged tool family — scheduler/reminder
        // owner-scoping below, vault.delete/set/move, plus agent.*, role.*, and
        // system.{disable,uninstall}_skill further down — so there is exactly
        // one "is the caller a SystemAdmin" decision point. See `crate::authz`.
        let is_admin = crate::authz::make_admin_policy(Arc::clone(&identity_store_handle));
        Self {
            identity_store_handle,
            is_admin,
        }
    }
}

async fn register_document_store_tools(config: &WorkforceConfig, toolbox: &Toolbox, admin: &Admin) {
    let Toolbox { tools, .. } = toolbox;
    let Admin { is_admin, .. } = admin;
    // The document store: scoped JSON persistence for workflows, skills
    // and agents. In-memory when Postgres is off — the factory says so.
    match crate::store::build_document_store(config).await {
        Ok(store) => {
            crate::store_tool::register(tools, Arc::clone(&store), Arc::clone(is_admin));
            crate::pipeline_tool::register(tools, Arc::clone(&store), Arc::clone(is_admin));
            crate::store_bridge::HubSkillDocumentStore::install(
                store,
                tokio::runtime::Handle::current(),
            );
            info!("Document store tools registered");
        }
        Err(e) => {
            warn!(error = %e, "Document store unavailable — store.* tools not registered")
        }
    }
}

async fn prepare_scheduler(
    setup: &Setup,
    toolbox: &Toolbox,
    admin: &Admin,
    scheduler: Option<&Arc<Scheduler>>,
) -> Result<()> {
    let Setup {
        effective_memory, ..
    } = setup;
    let Toolbox { tools, .. } = toolbox;
    let Admin { is_admin, .. } = admin;
    // Register scheduler tool if available
    if let Some(sched) = scheduler {
        // Load persisted jobs BEFORE registering built-in jobs.
        // This ensures user-created jobs (reminders, scheduled tasks) survive restarts.
        sched.load_jobs().await?;

        tools.register(Arc::new(
            SchedulerTool::new(Arc::clone(sched)).with_admin_policy(Arc::clone(is_admin)),
        ));
        tools.register(Arc::new(
            ReminderTool::new(Arc::clone(sched)).with_admin_policy(Arc::clone(is_admin)),
        ));
        info!("Scheduler and reminder tools registered");

        // Knowledge re-indexing is no longer an LLM agent turn — it runs as
        // a deterministic background pass (`MaintenanceRunner::run_pass`,
        // wired in `cli::main`). Drop any `knowledge-maintenance` job left
        // persisted by an older build, else `load_jobs` above resurrects it.
        if let Some(job) = sched
            .list_jobs()
            .await
            .into_iter()
            .find(|j| j.name == "knowledge-maintenance")
        {
            if sched.remove_job(&job.id).await {
                info!(job_id = %job.id, "Removed obsolete knowledge-maintenance job");
            }
        }

        // Each person's memories are consolidated inside the agent's own
        // consolidation pass; drop the per-person jobs older builds kept,
        // which would consolidate everyone twice.
        for job in sched.list_jobs().await.into_iter().filter(|j| {
            matches!(
                j.payload,
                crate::scheduler::JobPayload::Consolidation {
                    identity_id: Some(_)
                }
            )
        }) {
            if sched.remove_job(&job.id).await {
                info!(job_id = %job.id, "Removed per-person memory consolidation job");
            }
        }

        // Register memory consolidation job if enabled
        if effective_memory.consolidation {
            match crate::memory_consolidation::register_consolidation_job(
                sched.as_ref(),
                Some(effective_memory.consolidation_interval_secs),
            )
            .await
            {
                Ok(job_id) => {
                    info!(job_id = %job_id, "Memory consolidation job registered")
                }
                Err(e) => {
                    tracing::warn!("Failed to register memory consolidation: {}", e)
                }
            }
        }
    }
    Ok(())
}

impl Vaults {
    fn register(
        config: &WorkforceConfig,
        credentials: &Credentials,
        admin: &Admin,
        toolbox: &Toolbox,
    ) -> Self {
        let Credentials {
            vault,
            global_vault,
            logins,
            ..
        } = credentials;
        let Admin {
            identity_store_handle,
            is_admin,
            ..
        } = admin;
        let Toolbox {
            tools,
            reload_image_tool,
            ..
        } = toolbox;
        // Register vault tools. Hub-served (cloud) deployments redact
        // secret values returned to the LLM by default — built-in
        // adapters and WASM skills bypass this layer for HTTP auth.
        let agent_registry: crate::hosts::RegistryCell = Arc::default();
        let known_vaults = crate::subscription::store::known_vaults(
            vault.clone(),
            global_vault.clone(),
            Arc::clone(&agent_registry),
        );
        if let Some(ref v) = vault {
            let redaction = crate::vault_tool::RedactionPolicy::default_cloud();
            let concrete = Arc::new(crate::user_config::LayeredUserVault::new(
                Arc::clone(v),
                global_vault.clone(),
            ));
            let login_writes = crate::subscription::store::LoginWrites::new(
                Arc::clone(&concrete),
                Arc::clone(&known_vaults),
                {
                    let logins = Arc::clone(logins);
                    let is_admin = Arc::clone(is_admin);
                    Arc::new(move || {
                        logins.may_share(
                            crate::user_config::current_identity_id().as_deref(),
                            is_admin(),
                        )
                    })
                },
            );
            let layered: Arc<dyn workforce_common::settings::SecretStore> = concrete;
            let owner_resolver = make_owner_resolver(Arc::clone(identity_store_handle));

            tools.register(Arc::new(VaultGetTool::with_redaction(
                Arc::clone(&layered),
                redaction.clone(),
            )));
            tools.register(Arc::new(
                VaultSetTool::new(Arc::clone(&layered))
                    .with_admin_policy(Arc::clone(is_admin))
                    .with_reload_image_tool(Arc::clone(reload_image_tool))
                    .with_logins(login_writes.clone()),
            ));
            tools.register(Arc::new(VaultListTool::new(Arc::clone(&layered))));
            tools.register(Arc::new(
                VaultDeleteTool::new(Arc::clone(&layered))
                    .with_admin_policy(Arc::clone(is_admin))
                    .with_reload_image_tool(Arc::clone(reload_image_tool))
                    .with_logins(login_writes.clone()),
            ));
            tools.register(Arc::new(VaultInspectTool::with_redaction(
                Arc::clone(&layered),
                redaction,
            )));
            tools.register(Arc::new(crate::vault_tool::VaultMyCredentialsTool::new(
                Arc::clone(&layered),
                Arc::clone(&owner_resolver),
            )));
            tools.register(Arc::new(
                crate::vault_tool::VaultMoveTool::new(Arc::clone(&layered), Arc::clone(is_admin))
                    .with_logins(login_writes.clone()),
            ));
            tools.register(Arc::new(
                crate::vault_tool::VaultRenameTool::new(Arc::clone(&layered), Arc::clone(is_admin))
                    .with_logins(login_writes),
            ));
            tools.register(Arc::new(crate::vault_tool::VaultCopyTool::new(
                Arc::clone(&layered),
                Arc::clone(is_admin),
            )));
            info!("Vault tools registered (LLM-side redaction = redact)");

            // Direct database-access tools (database.*) — an essential built-in
            // family resolving connection strings from the layered vault
            // (`databases/` namespace, denied to vault.* reads). `database.execute`
            // is gated by the same SystemAdmin policy as the vault write tools.
            workforce_agent::tools::database::register_database_tools(
                tools,
                Arc::clone(&layered),
                Arc::clone(is_admin),
            );
            info!("Database tools registered (database.*)");

            // Generic outbound HTTP tools (http.*) — an essential built-in family
            // resolving endpoint base URLs and credentials from the layered vault
            // (`http/` namespace, denied to vault.* reads). State-changing methods
            // are gated by `Capability::NetworkWrite` at the pre-dispatch cap gate,
            // not by an inline admin check, so an admin can delegate them per role
            // from the capability matrix.
            for tool in workforce_agent::tools::http::http_tools(Arc::clone(&layered)) {
                tools.register(tool);
            }
            info!("HTTP tools registered (http.*)");

            // Slack scaffolding tools — `vault` for operator-level workspace
            // tokens (bot/app/signing); `layered` for per-user xoxp-… tokens.
            let slack_ctx = crate::slack_tool::SlackToolContext {
                vault: Arc::clone(v),
                secret_store: Arc::clone(&layered),
                data_dir: config.hub.data_dir.clone(),
            };
            for tool in crate::slack_tool::slack_tools(slack_ctx) {
                tools.register(tool);
            }
            info!("Slack scaffolding tools registered");
        }

        // `secret.list` lets the agent discover `/secret`-set credentials;
        // read-only and vault-independent, so register it unconditionally.
        tools.register(Arc::new(crate::secret_tool::SecretListTool));
        Vaults {
            agent_registry,
            known_vaults,
        }
    }
}

/// Surfaces are added later, through `register_surface`.
fn register_reaction_tools(
    toolbox: &Toolbox,
    surface_registry: &Arc<tokio::sync::RwLock<SurfaceRegistry>>,
) {
    let Toolbox { tools, .. } = toolbox;
    tools.register(Arc::new(ReactionTool::new(Arc::clone(surface_registry))));
    tools.register(Arc::new(ReactionRemoveTool::new(Arc::clone(
        surface_registry,
    ))));
    info!("Reaction tools registered");
}

fn install_media_tools(setup: &Setup, credentials: &Credentials, toolbox: &Toolbox) {
    let Setup {
        agent_id,
        effective_llm,
        ..
    } = setup;
    let Credentials {
        vault,
        global_vault,
        ..
    } = credentials;
    let Toolbox {
        tools,
        image_config,
        video_config,
        speech_config,
        speech_home,
        ..
    } = toolbox;
    // Register the image-generation tool only when [llm.image] is present
    // and enabled — strictly opt-in per install. Generated images are
    // staged as artifacts and delivered to the originating surface by the
    // runner's image sink (installed in `install_tool_access`). The same
    // path is reused by `reload_config` to hot-swap the provider on edits.
    // On a build failure the recorded config is `None` so the next reload
    // retries rather than short-circuiting on an unchanged broken block.
    let image_outcome = workforce_agent::tools::image_gen::install_image_tool(
        tools,
        agent_id.as_str(),
        None,
        effective_llm.image.as_ref(),
        crate::credentials::image_keys(effective_llm.image.as_ref(), vault, global_vault),
    );
    image_outcome.log(agent_id.as_str());
    *image_config.write() = image_outcome.recorded(effective_llm.image.clone());

    let video_outcome = workforce_agent::tools::video_gen::install_video_tool(
        tools,
        agent_id.as_str(),
        None,
        effective_llm.video.as_ref(),
        crate::credentials::video_keys(effective_llm.video.as_ref(), vault, global_vault),
    );
    video_outcome.log(agent_id.as_str());
    *video_config.write() = video_outcome.recorded(effective_llm.video.clone());

    let speech_outcome = workforce_agent::tools::speech_gen::install_speech_tool(
        tools,
        agent_id.as_str(),
        None,
        effective_llm.speech.as_ref(),
        speech_home,
        crate::credentials::speech_keys(effective_llm.speech.as_ref(), vault, global_vault),
        Some(Arc::new(crate::hub_core::formatting::normalize_for_tts)),
        Some(crate::vault::speech_user_voice_resolver(
            vault.clone(),
            global_vault.clone(),
        )),
    );
    speech_outcome.log(agent_id.as_str());
    *speech_config.write() = speech_outcome.recorded(effective_llm.speech.clone());

    workforce_agent::tools::video_compose::install_video_tools(tools, agent_id.as_str());
}

fn register_messaging_tools(
    setup: &Setup,
    stores: &Stores,
    toolbox: &Toolbox,
    admin: &Admin,
    surface_registry: &Arc<tokio::sync::RwLock<SurfaceRegistry>>,
) {
    let Setup { agent_id, .. } = setup;
    let Stores { sessions, .. } = stores;
    let Toolbox { tools, .. } = toolbox;
    let Admin {
        identity_store_handle,
        ..
    } = admin;
    // Register messaging tools (surfaces are added later via register_surface)
    tools.register(Arc::new(MessagingSendTool::new(Arc::clone(
        surface_registry,
    ))));
    tools.register(Arc::new(MessagingReadTool::new(Arc::clone(
        surface_registry,
    ))));
    tools.register(Arc::new(MessagingEditTool::new(Arc::clone(
        surface_registry,
    ))));
    tools.register(Arc::new(MessagingDeleteTool::new(Arc::clone(
        surface_registry,
    ))));
    tools.register(Arc::new(MessagingReadThreadTool::new(Arc::clone(
        surface_registry,
    ))));
    tools.register(Arc::new(ViewAttachmentTool::new(Arc::clone(
        surface_registry,
    ))));
    tools.register(Arc::new(GetUserPhotoTool::new(Arc::clone(
        surface_registry,
    ))));
    // Training tool registration moved below — depends on
    // the describer, which is built after this tool block.
    tools.register(Arc::new(MessagingPinTool::new(Arc::clone(
        surface_registry,
    ))));
    tools.register(Arc::new(MessagingUnpinTool::new(Arc::clone(
        surface_registry,
    ))));
    tools.register(Arc::new(MessagingListPinsTool::new(Arc::clone(
        surface_registry,
    ))));
    tools.register(Arc::new(MessagingMemberInfoTool::new(Arc::clone(
        surface_registry,
    ))));
    tools.register(Arc::new(MessagingEmojiListTool::new(Arc::clone(
        surface_registry,
    ))));
    tools.register(Arc::new(MessagingUploadFileTool::new(Arc::clone(
        surface_registry,
    ))));
    tools.register(Arc::new(MessagingSendFileTool::new(
        Arc::clone(surface_registry),
        agent_id.as_str(),
    )));
    tools.register(Arc::new(MessagingChannelInfoTool::new(Arc::clone(
        surface_registry,
    ))));
    tools.register(Arc::new(crate::whoami_tool::WhoAmITool::new(Arc::clone(
        identity_store_handle,
    ))));
    tools.register(Arc::new(crate::identity_tool::WhoAmIIdentityTool::new(
        Arc::clone(identity_store_handle),
    )));
    tools.register(Arc::new(
        crate::identity_lifecycle_tool::IdentityLifecycleTool,
    ));
    info!("Messaging tools registered");

    // Transcript-search fallback (per-user scoped at the tool level).
    tools.register(Arc::new(crate::sessions_tool::SessionsTool::new(
        Arc::clone(sessions),
    )));

    // tool.retrieve — recover originals of tool outputs that compaction
    // truncated/evicted.
    tools.register(Arc::new(crate::retrieve_tool::RetrieveTool::new(
        Arc::clone(sessions),
    )));
}

async fn register_artifact_tools(
    config: &WorkforceConfig,
    setup: &Setup,
    stores: &Stores,
    toolbox: &Toolbox,
) {
    let Setup {
        agent_id,
        workspace,
        ..
    } = setup;
    let Stores { sessions, .. } = stores;
    let Toolbox { tools, .. } = toolbox;
    // ─── Artifact Tools (Artifacts v2: DB-backed, file fallback) ───
    let artifact_backend: Option<Arc<dyn workforce_orchestrator::artifact_store::ArtifactBackend>> =
        match shared_artifact_store(config).await {
            Some(store) => {
                Some(store as Arc<dyn workforce_orchestrator::artifact_store::ArtifactBackend>)
            }
            None => match workforce_orchestrator::artifact_store::FsArtifactBackend::create_at(
                workspace,
            ) {
                Ok(be) => Some(Arc::new(be)),
                Err(e) => {
                    warn!(error = ?e, "Artifact file backend init failed — skipping artifact tool");
                    None
                }
            },
        };
    if let Some(backend) = artifact_backend {
        match crate::artifact_tool::ArtifactTool::new(backend, workspace.clone(), agent_id.clone())
        {
            Ok(tool) => tools.register(Arc::new(tool)),
            Err(e) => warn!(error = ?e, "ArtifactTool init failed — skipping"),
        }
    }

    // ─── Session Label Tool ────────────────────────────────────────────
    tools.register(Arc::new(crate::session_tool::SessionLabelTool::new(
        Arc::clone(sessions),
    )));

    // Engagement-mode tools are registered per-agent against the one shared
    // hub store by the gateway wiring (`engagement_tool::register_engagement_tools`),
    // so a `set_mode` applies only to the agent that called it.
    info!("Handoff, artifact, and session tools registered");
}

impl Overrides {
    async fn load(setup: &Setup, credentials: &Credentials, llm_stack: &LlmStack) -> Result<Self> {
        let Setup {
            effective_llm,
            config_equivalences,
            provider_kind_val,
            tier_key_val,
            model,
            ..
        } = setup;
        let provider_kind_val = *provider_kind_val;
        let Credentials {
            vault,
            global_vault,
            ..
        } = credentials;
        let LlmStack {
            pool, failover_ref, ..
        } = llm_stack;
        // ─── Load Additional LLM Tokens from Vault ────────────────────────
        let active_model: Arc<parking_lot::RwLock<Option<String>>> =
            Arc::new(parking_lot::RwLock::new(None));
        let active_max_tokens: Arc<parking_lot::RwLock<Option<u32>>> =
            Arc::new(parking_lot::RwLock::new(None));
        let active_max_rounds: Arc<parking_lot::RwLock<Option<u32>>> =
            Arc::new(parking_lot::RwLock::new(None));
        let active_compaction: Arc<
            parking_lot::RwLock<Option<workforce_common::session::CompactionConfig>>,
        > = Arc::new(parking_lot::RwLock::new(None));
        let active_routing: Arc<
            parking_lot::RwLock<Option<workforce_agent::model_router::RoutingConfig>>,
        > = Arc::new(parking_lot::RwLock::new(None));

        // `[llm.routing]` from config.toml lives in `resolved.routing` (hot-
        // reloaded); `active_routing` is the runtime override only, seeded from
        // vault below and set by the routing tool. `current_routing_config`
        // layers override over the resolved config.
        if let Some(ref v) = vault {
            // Load active model override from vault, normalising any stored
            // tier name through the registry so the in-memory value is always
            // a concrete model id from the moment it loads.
            if let Some(model_override) = v.get("llm/active_model") {
                let tiers =
                    workforce_agent::llm::tiers::TierRegistry::new(config_equivalences.as_ref());
                let resolved = tiers.resolve_model(&model_override, tier_key_val);
                if resolved != model_override {
                    info!(
                        stored = %model_override,
                        resolved = %resolved,
                        "Loaded active model override from vault; resolved tier name"
                    );
                } else {
                    info!(model = %model_override, "Loaded active model override from vault");
                }
                *active_model.write() = Some(resolved);
            }

            // Load max_tokens/max_rounds overrides from vault
            if let Some(val) = v.get("llm/max_tokens") {
                if let Ok(n) = val.parse::<u32>() {
                    info!(max_tokens = n, "Loaded max_tokens override from vault");
                    *active_max_tokens.write() = Some(n);
                }
            }
            if let Some(val) = v.get("llm/max_rounds") {
                if let Ok(n) = val.parse::<u32>() {
                    info!(max_rounds = n, "Loaded max_rounds override from vault");
                    *active_max_rounds.write() = Some(n);
                }
            }

            // Restore compaction config override from vault
            if let Some(config_json) = v.get("compaction/config") {
                match serde_json::from_str::<workforce_common::session::CompactionConfig>(
                    &config_json,
                ) {
                    Ok(config) => {
                        info!(
                            keep_recent = config.keep_recent_messages,
                            prune_max_chars = config.prune_tool_output_max_chars,
                            compaction_model = ?config.compaction_model,
                            "Compaction config restored from vault"
                        );
                        *active_compaction.write() = Some(config);
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to parse compaction config from vault");
                    }
                }
            }

            // Restore routing config override from vault
            if let Some(routing_json) = v.get("llm/active_routing") {
                match serde_json::from_str::<workforce_agent::model_router::RoutingConfig>(
                    &routing_json,
                ) {
                    Ok(config) => {
                        info!(
                            enabled = config.enable_adaptive_routing,
                            "Routing config restored from vault"
                        );
                        *active_routing.write() = Some(config);
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to parse routing config from vault");
                    }
                }
            }

            // Load additional tokens from vault (legacy path)
            let token_paths = v.list(Some("llm/tokens/"));
            for path in &token_paths {
                let name = path.strip_prefix("llm/tokens/").unwrap_or(path);
                if name == "default" {
                    continue; // Already loaded from env
                }
                if provider_kind_val.is_subscription() {
                    warn!(
                        name,
                        provider = provider_kind_val.canonical_name(),
                        "LLM token ignored: {}",
                        crate::subscription::LOGINS_ARE_NOT_ACCOUNTS
                    );
                    continue;
                }
                if let Some(token) = v.get(path) {
                    match create_named_provider_async(
                        provider_kind_val,
                        name,
                        &token,
                        Some(model),
                        effective_llm.thinking_budget_tokens,
                        effective_llm.zai_subscription,
                    )
                    .await
                    {
                        Ok(provider) => {
                            pool.add(name, Arc::from(provider)).await;
                            info!(name = name, "Additional LLM token loaded from vault");
                        }
                        Err(e) => {
                            warn!(
                                name = name,
                                error = %e,
                                "Failed to load additional LLM token from vault"
                            );
                        }
                    }
                }
            }

            // Load accounts from vault (new unified path). Agent-vault
            // accounts first, then global-vault accounts under paths the
            // agent doesn't define — an account moved to the global tier
            // (gateway "move to global") keeps feeding the pool/failover.
            let mut account_sources: Vec<(&Arc<Vault>, String)> = v
                .list(Some("llm/accounts/"))
                .into_iter()
                .map(|p| (v, p))
                .collect();
            if let Some(ref g) = global_vault {
                let agent_paths: std::collections::HashSet<&str> =
                    account_sources.iter().map(|(_, p)| p.as_str()).collect();
                let global_paths: Vec<String> = g
                    .list(Some("llm/accounts/"))
                    .into_iter()
                    .filter(|p| !agent_paths.contains(p.as_str()))
                    .collect();
                account_sources.extend(global_paths.into_iter().map(|p| (g, p)));
            }
            for (src, path) in &account_sources {
                let name = path.strip_prefix("llm/accounts/").unwrap_or(path);
                if let Some(data_str) = src.get(path) {
                    let place = format!("{} {path}", src.location());
                    let Some(data) = serde_json::from_str::<serde_json::Value>(&data_str)
                        .ok()
                        .filter(serde_json::Value::is_object)
                    else {
                        crate::agent_llm::account_kind(&place, name, data_str.trim())?;
                        continue;
                    };
                    let acct_provider = data
                        .get("provider")
                        .and_then(|p| p.as_str())
                        .unwrap_or("unknown");
                    let acct_key = data.get("api_key").and_then(|k| k.as_str()).unwrap_or("");
                    if acct_key.is_empty() {
                        continue;
                    }
                    let acct_kind =
                        crate::agent_llm::account_kind(&place, acct_provider, acct_key)?;
                    if acct_kind.is_subscription() {
                        warn!(
                            name,
                            provider = acct_provider,
                            "Account ignored: {}",
                            crate::subscription::LOGINS_ARE_NOT_ACCOUNTS
                        );
                        continue;
                    }
                    let is_primary = crate::agent_llm::AgentProviders::pools_as_primary(
                        provider_kind_val,
                        acct_kind,
                    );
                    match create_named_provider_async(
                        acct_kind,
                        name,
                        acct_key,
                        Some(model),
                        if is_primary {
                            effective_llm.thinking_budget_tokens
                        } else {
                            None
                        },
                        effective_llm.zai_subscription,
                    )
                    .await
                    {
                        Ok(provider) => {
                            let provider_arc: Arc<dyn workforce_agent::llm::provider::LlmProvider> =
                                Arc::from(provider);
                            if is_primary {
                                pool.add(name, provider_arc).await;
                                info!(
                                    name = name,
                                    provider = acct_provider,
                                    "Account loaded from vault into pool"
                                );
                            } else if let Some(ref fp) = failover_ref {
                                fp.add_external_provider(acct_provider, provider_arc).await;
                                info!(
                                    name = name,
                                    provider = acct_provider,
                                    "External account loaded from vault"
                                );
                            }
                        }
                        Err(e) => {
                            warn!(
                                name = name,
                                provider = acct_provider,
                                error = %e,
                                "Failed to load account from vault"
                            );
                        }
                    }
                }
            }

            // Restore failover chain from vault if previously saved
            if let Some(ref fp) = failover_ref {
                if let Some(chain_json) = v.get("llm/failover_chain") {
                    match serde_json::from_str::<Vec<String>>(&chain_json) {
                        Ok(chain) => {
                            fp.set_chain(chain.clone()).await;
                            info!(chain = ?chain, "Failover chain restored from vault");
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to parse failover chain from vault, using config defaults");
                        }
                    }
                }
            }
        }
        Ok(Self {
            active_model,
            active_max_tokens,
            active_max_rounds,
            active_compaction,
            active_routing,
        })
    }
}

async fn probe_credentials(llm_stack: &LlmStack) {
    let LlmStack {
        pool, failover_ref, ..
    } = llm_stack;
    // Probe every entry's credentials so bad keys land in cooldown (pool)
    // or are dropped (failover) before the first user-facing request burns
    // round-trips on a doomed token.
    //
    // Time-boxed as a whole, on top of the per-provider timeouts, because
    // this runs before the server binds its port: anything that can hang
    // here stops the hub starting at all. That is not hypothetical — an
    // unbounded probe against a provider that accepted connections and then
    // never replied wedged startup, failed the health check, and rolled
    // back every deploy for hours. A probe is an optimisation; it must
    // never be the reason the hub does not come up.
    let probes = async {
        let pool_probe = pool.probe_all_entries();
        let failover_probe = async {
            if let Some(ref fp) = failover_ref {
                fp.probe_all_external_providers().await;
            }
        };
        tokio::join!(pool_probe, failover_probe);
    };
    if tokio::time::timeout(CREDENTIAL_PROBE_BUDGET, probes)
        .await
        .is_err()
    {
        warn!(
            budget_secs = CREDENTIAL_PROBE_BUDGET.as_secs(),
            "Credential probes did not finish in time; starting anyway —                  a bad key will surface on first use instead"
        );
    }
}

fn migrate_vault_config(setup: &Setup, credentials: &Credentials) {
    let Setup {
        agent_id,
        workspace,
        admission,
        ..
    } = setup;
    let Credentials { vault, .. } = credentials;
    // ─── Vault → Workspace Config Migration ─────────────────────────────
    // Migrate legacy vault config overrides to workspace config.toml.
    // This runs once: if vault has config entries, they're written to
    // config.toml and removed from vault. Secrets (llm/tokens/*, llm/accounts/*)
    // stay in vault.
    if let Some(ref v) = vault {
        let vault_config_keys = [
            "llm/active_model",
            "llm/max_tokens",
            "llm/max_rounds",
            "llm/failover_chain",
            "llm/active_routing",
            "compaction/config",
        ];
        let has_vault_config = vault_config_keys.iter().any(|k| v.get(k).is_some());

        if has_vault_config {
            info!(agent = %agent_id, "Migrating vault config overrides to workspace config.toml");
            // Read all vault config values first, then write to workspace config
            let vault_model = v.get("llm/active_model");
            let vault_max_tokens = v.get("llm/max_tokens");
            let vault_max_rounds = v.get("llm/max_rounds");
            let vault_failover = v.get("llm/failover_chain");
            let vault_routing = v.get("llm/active_routing");
            let vault_compaction = v.get("compaction/config");

            if let Err(e) = workforce_common::settings::AgentWorkspaceConfig::update(
                workspace,
                admission,
                |cfg| {
                    let llm = cfg.llm.get_or_insert_with(Default::default);

                    if let Some(ref model) = vault_model {
                        llm.default_model = Some(model.clone());
                    }
                    if let Some(ref val) = vault_max_tokens {
                        if let Ok(n) = val.parse::<u32>() {
                            llm.max_tokens = Some(n);
                        }
                    }
                    if let Some(ref val) = vault_max_rounds {
                        if let Ok(n) = val.parse::<u32>() {
                            llm.max_rounds = Some(n);
                        }
                    }
                    if let Some(ref chain_json) = vault_failover {
                        if let Ok(chain) = serde_json::from_str::<Vec<String>>(chain_json) {
                            llm.failover = Some(chain);
                        }
                    }
                    if let Some(ref routing_json) = vault_routing {
                        if let Ok(rc) = serde_json::from_str::<
                            workforce_agent::model_router::RoutingConfig,
                        >(routing_json)
                        {
                            let llm = cfg.llm.get_or_insert_with(Default::default);
                            llm.routing =
                                Some(workforce_common::settings::WorkspaceRoutingConfig {
                                    enable_adaptive_routing: Some(rc.enable_adaptive_routing),
                                    investigation_switch_round: Some(rc.investigation_switch_round),
                                    coding_switch_round: Some(rc.coding_switch_round),
                                    read_only_threshold: Some(rc.read_only_threshold),
                                    fast_round_threshold_ms: Some(rc.fast_round_threshold_ms),
                                    escalate_on_haiku_failure: Some(rc.escalate_on_haiku_failure),
                                });
                        }
                    }
                    if let Some(ref comp_json) = vault_compaction {
                        if let Ok(comp) = serde_json::from_str::<
                            workforce_common::session::CompactionConfig,
                        >(comp_json)
                        {
                            let c = cfg.compaction.get_or_insert_with(Default::default);
                            c.max_context_tokens = Some(comp.max_context_tokens);
                            c.compaction_threshold = Some(comp.compaction_threshold);
                            c.keep_recent_messages = Some(comp.keep_recent_messages);
                            c.enable_memory_flush = Some(comp.enable_memory_flush);
                            c.prune_tool_outputs = Some(comp.prune_tool_outputs);
                            c.prune_tool_output_max_chars = Some(comp.prune_tool_output_max_chars);
                            c.max_compaction_passes = Some(comp.max_compaction_passes);
                            c.min_messages_to_compact = Some(comp.min_messages_to_compact);
                            c.urgent_compaction_threshold = Some(comp.urgent_compaction_threshold);
                            c.compaction_model = comp.compaction_model.clone();
                        }
                    }
                },
            ) {
                warn!(error = %e, "Failed to migrate vault config to workspace config.toml");
            } else {
                // Clean up migrated config entries from vault (keep secrets)
                for key in &vault_config_keys {
                    let _ = v.delete(key);
                }
                info!(agent = %agent_id, "Vault config overrides migrated to workspace config.toml");
            }
        }
    }
}

impl Skills {
    async fn register(
        config: &WorkforceConfig,
        setup: &Setup,
        credentials: &Credentials,
        llm_stack: &LlmStack,
        toolbox: &Toolbox,
        stores: &Stores,
        admin: &Admin,
        vaults: &Vaults,
        overrides: &Overrides,
        memory_metrics: &mut crate::memory_metrics::MemoryMetrics,
    ) -> Result<Self> {
        let Setup {
            agent_id,
            workspace,
            identity,
            effective_llm,
            config_equivalences_cell,
            provider_kind,
            tier_key,
            admission,
            resolved,
            original_default_model,
            current_default_model,
            ..
        } = setup;
        let Credentials {
            vault,
            global_vault,
            team_store_handle,
            providers,
            ..
        } = credentials;
        let LlmStack {
            pool, failover_ref, ..
        } = llm_stack;
        let Toolbox {
            runner,
            tool_paths,
            tools,
            reload_image_tool,
            ..
        } = toolbox;
        let Stores { sessions, .. } = stores;
        let Admin {
            identity_store_handle,
            is_admin,
            ..
        } = admin;
        let Vaults { agent_registry, .. } = vaults;
        let Overrides {
            active_model,
            active_max_tokens,
            active_max_rounds,
            active_routing,
            ..
        } = overrides;
        // ─── Skill Loading ────────────────────────────────────────────────
        // `skill_state` is in-memory, so a redeploy starts empty. Built-in
        // skills whose shared credentials are configured are auto-loaded just
        // below (`[skills] auto_load`, default on) so they survive a restart;
        // anything not auto-loaded (per-user/OAuth, or auto_load off) the agent
        // discovers via the system-prompt hint and loads on demand with
        // `system.enable_skill` (DW-277 deferred loading) to save context.
        let skill_state = Arc::new(crate::hub_core::skill_state::SkillStateTracker::new());
        if config.skills.auto_load {
            info!("Configured skills will be auto-loaded at startup (use system.enable_skill for the rest)");
        } else {
            info!("Skills deferred to on-demand loading (use system.enable_skill)");
        }

        // Register logs tool (policy controls access via "system.logs")
        tools.register(Arc::new(crate::logs_tool::SystemLogsTool));

        let discovered_skills = crate::external_skills::ExternalSkills::shared(
            &config.skills.resolved_skills_dir(&config.hub.data_dir),
        );

        // This agent's `wf_tool_invoke` bridge slot: skill tools register
        // before the policy engine (which the invoker needs) exists, so they
        // hold the slot and it is filled below, after `load_policy`.
        let wasm_invoker_slot: workforce_wasm_runtime::types::SkillInvokerSlot =
            Arc::new(parking_lot::RwLock::new(None));

        // The hub and every agent share ONE telemetry store (one Postgres pool)
        // via the process-wide cell in `state::shared_telemetry_store`. `true`
        // skips the availability gate and lets the Postgres connection itself
        // decide — failure is fail-closed (no SQLite degrade).
        let telemetry_store = crate::state::shared_telemetry_store(config, true).await?;
        let cost_tracker = Arc::new(CostTracker::with_store(Arc::clone(&telemetry_store)));
        crate::artifact_index::cost_tagging_to(Arc::clone(&cost_tracker));

        // Late-bound from `cli::main` (see `set_permission_evaluator`) and
        // built below; created here so `know_how` reads them per call.
        let permission_evaluator = Arc::new(parking_lot::RwLock::new(None));
        let policy_cell: Arc<std::sync::OnceLock<Arc<PolicyEngine>>> = Arc::default();

        // Register system tools (always — policy controls access)
        if let Some(ref v) = vault {
            let sys_ctx = crate::system_tool::SystemToolContext {
                vault: Arc::clone(v),
                global_vault: global_vault.clone(),
                tools: Arc::clone(tools),
                skill_state: Arc::clone(&skill_state),
                config: config.clone(),
                discovered_skills: Arc::clone(&discovered_skills),
                admin: Arc::clone(is_admin),
                teams: Arc::clone(team_store_handle),
                path_roots: tool_paths.clone(),
                // The capability alone, deliberately: unlike the REST and chat
                // surfaces, this one is reachable by prompt injection inside the
                // caller's turn, so the bypass over everyone's external skills
                // stays something an operator can withhold in the matrix rather
                // than something a SystemAdmin carries by office.
                skill_managers: crate::authz::make_capability_policy(
                    Arc::clone(identity_store_handle),
                    crate::authz::Capability::SkillsManage,
                ),
            };
            for tool in crate::system_tool::system_tools(sys_ctx) {
                tools.register(tool);
            }

            if let Some(tool) = crate::decide_tool::decide_tool(vault.clone(), global_vault.clone())
            {
                tools.register(tool);
            }

            // `know_how`: what the agent has learned about where its data and
            // code live. Registered unless the install turns know-how off;
            // whether this agent has it is read per call from its own config.
            if config.know_how.mode != workforce_common::settings::ProgramsMode::Off {
                let install = config.know_how.clone();
                let cell = resolved.clone();
                let tool_vault = vault.clone();
                let tool_global = global_vault.clone();
                let tool_registry = Arc::clone(tools);
                let tool_evaluator = Arc::clone(&permission_evaluator);
                let tool_policy = Arc::clone(&policy_cell);
                let tool_agent = agent_id.as_str().to_string();
                let tool_admits = crate::hosts::admits_through(Arc::clone(agent_registry));
                tools.register(Arc::new(crate::know_how::tool::KnowHowTool::new(
                    crate::know_how::KnowHow::shared(config).await,
                    agent_id.as_str(),
                    Arc::new(move || install.for_workspace(cell.snapshot().workspace.as_ref())),
                    Arc::new(move || {
                        let (instances, default) = tool_vault
                            .clone()
                            .map(|vault| {
                                crate::skill_loader::metabase_sites_for(vault, tool_global.clone())
                            })
                            .unwrap_or_default();
                        crate::know_how::Access {
                            sites: crate::know_how::Sites {
                                instances,
                                default,
                                pinned: workforce_agent::runner::current_active_skill_instances()
                                    .get("metabase")
                                    .and_then(|set| set.first().cloned()),
                            },
                            repos: crate::know_how::code::readable_repos(
                                crate::know_how::code::code_index(),
                                &tool_registry,
                                tool_evaluator.read().clone(),
                                &tool_agent,
                            ),
                            policy: tool_policy.get().cloned(),
                            admits: Some(Arc::clone(&tool_admits)),
                        }
                    }),
                    Arc::clone(is_admin) as crate::know_how::tool::AdminFn,
                    {
                        let sessions = Arc::clone(sessions);
                        Arc::new(move || {
                            workforce_agent::runner::CURRENT_SESSION_ID
                                .try_with(|id| id.clone())
                                .ok()
                                .and_then(|id| sessions.get(&id).ok().flatten())
                                .is_none_or(|s| s.is_shared_conversation())
                        })
                    },
                )));
            }

            // Register WASM skill management tools (if engine is available)
            {
                let data_dir = std::path::PathBuf::from(&config.hub.data_dir);
                let wasm_store_path = data_dir.join("wasm-skills");
                // ONE engine and ONE store per process: several agent services
                // run this block, and separate instances would give each its
                // own module cache and install index — an install through one
                // agent would then be invisible to the others (and to
                // `cmd: skill grant`) until the next reboot. Per-agent context
                // (locker vault, tool invoker) rides on each registered tool.
                if let Some(engine) = crate::wasm_skill_tools::global_skill_engine() {
                    if crate::wasm_skill_tools::engine_not_yet_metered() {
                        memory_metrics.add_wasm_engine("wasm-skills", Arc::clone(&engine));
                    }

                    // The skill locker: the agent-independent global vault when
                    // present, with the agent vault as legacy read fallback.
                    // Paths arrive fully qualified (`wasm-skills/{skill}/…`),
                    // so no `LayeredUserVault` wrapper, which would
                    // double-prefix the per-user segment.
                    let locker = crate::wasm_skill_tools::SkillLockerVault::compose(
                        global_vault.clone(),
                        Some(Arc::clone(v)),
                    );

                    let store = crate::wasm_skill_tools::global_skill_store().or_else(|| {
                        match workforce_wasm_runtime::WasmSkillStore::open(&wasm_store_path) {
                            Ok(s) => {
                                let s = Arc::new(s);
                                crate::wasm_skill_tools::set_global_skill_store(Arc::clone(&s));
                                Some(s)
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "WASM skill store unavailable");
                                None
                            }
                        }
                    });
                    if let Some(store) = store {
                        let skill_hosts = crate::wasm_skill_tools::SkillHosts::process();
                        skill_hosts.register(
                            agent_id.as_str(),
                            tools,
                            &skill_state,
                            locker.clone(),
                            Arc::clone(&wasm_invoker_slot),
                        );
                        // Who may call a skill's tools — the authoritative check
                        // every registered skill tool runs. Wired here, where the
                        // store first exists, and shared by every agent.
                        crate::wasm_skill_tools::set_skill_use_gate(Arc::new(
                            crate::wasm_skill_access::HubSkillUseGate::new(
                                Arc::clone(&store),
                                Arc::clone(is_admin),
                                Arc::clone(team_store_handle),
                            ),
                        ));
                        // A build compiles arbitrary Rust: as the hub user it is
                        // admin-only, under the privilege-separated runner it is
                        // member-level authoring.
                        crate::wasm_skill_access::set_build_isolation(match runner.is_some() {
                            true => crate::wasm_skill_access::BuildIsolation::Sandboxed,
                            false => crate::wasm_skill_access::BuildIsolation::InProcess,
                        });
                        // Claude Code joins the other execution-class tools on
                        // the runner only when explicitly asked: dispatch is
                        // fail-closed, so a mistake takes out every coding
                        // session at once.
                        crate::claude_code::set_sandbox_runner(
                            config
                                .exec
                                .sandbox_claude_code
                                .then(|| runner.clone())
                                .flatten(),
                        );
                        let wasm_ctx = crate::wasm_skill_tools::WasmSkillContext {
                            engine: Arc::clone(&engine),
                            store: Arc::clone(&store),
                            tools: Arc::clone(tools),
                            skill_state: Arc::clone(&skill_state),
                            hosts: skill_hosts,
                            data_dir: data_dir.clone(),
                            policy: (&config.skills.wasm).into(),
                            agent_vault: Some(Arc::clone(v)),
                            global_vault: global_vault.clone(),
                            runner: runner.clone(),
                            // The same roots the filesystem and code tools use,
                            // so every tool that touches a skill project agrees
                            // on where it lives.
                            path_roots: tool_paths.clone(),
                            admin: Arc::clone(is_admin),
                            teams: Arc::clone(team_store_handle),
                            catalogs: config.skills.catalogs.clone(),
                        };
                        for tool in crate::wasm_skill_tools::wasm_skill_tools(wasm_ctx) {
                            tools.register(tool);
                        }

                        // Load previously installed WASM skills from disk
                        crate::wasm_skill_tools::load_installed_wasm_skills(
                            &engine,
                            &store,
                            tools,
                            &skill_state,
                            locker,
                            Arc::clone(&wasm_invoker_slot),
                        )
                        .await;

                        crate::wasm_skill_tools::assert_use_gate_wired();
                        info!("WASM skill tools registered");
                    }
                }
            }

            // Auto-load built-in skills whose shared credentials are already
            // configured, so a redeploy doesn't drop them (the in-memory
            // `skill_state` is rebuilt empty on boot — the cause of agents
            // telling users a configured skill "isn't set up" after a deploy).
            // Mirrors the WASM auto-load above; per-user/OAuth skills (google)
            // stay on-demand since there's no user scope at boot.
            if config.skills.auto_load {
                crate::skill_loader::auto_load_configured_skills(
                    v,
                    global_vault.as_ref(),
                    config,
                    tools,
                    &skill_state,
                );
            }

            // Register LLM management tools
            let llm_ctx = crate::llm_tool::LlmToolContext {
                vault: Arc::clone(v),
                global_vault: global_vault.clone(),
                pool: Arc::clone(pool),
                active_model: Arc::clone(active_model),
                active_max_tokens: Arc::clone(active_max_tokens),
                active_max_rounds: Arc::clone(active_max_rounds),
                provider_kind: Arc::clone(provider_kind),
                tier_key: Arc::clone(tier_key),
                llm_config: effective_llm.clone(),
                current_default_model: Arc::clone(current_default_model),
                original_default_model: Arc::clone(original_default_model),
                agent_id: agent_id.clone(),
                cost_tracker: Arc::clone(&cost_tracker),
                failover_provider: failover_ref.clone(),
                sessions: Arc::clone(sessions),
                active_routing: Arc::clone(active_routing),
                resolved: resolved.clone(),
                config_equivalences: Arc::clone(config_equivalences_cell),
                workspace: workspace.clone(),
                bedrock_catalog: bedrock_catalog(),
                reload_image_tool: Arc::clone(reload_image_tool),
                providers: Arc::clone(providers),
                admission: admission.clone(),
            };
            for tool in crate::llm_tool::llm_tools(llm_ctx) {
                tools.register(tool);
            }

            // Sentinel tools (security visibility)
            let sentinel_ctx = crate::sentinel_tool::SentinelToolContext {
                scan_history: Arc::new(tokio::sync::RwLock::new(std::collections::VecDeque::new())),
                identity: Arc::clone(identity),
            };
            for tool in crate::sentinel_tool::sentinel_tools(sentinel_ctx) {
                tools.register(tool);
            }

            info!("System tools registered (including LLM management and Sentinel)");
        }
        Ok(Self {
            skill_state,
            wasm_invoker_slot,
            telemetry_store,
            cost_tracker,
            permission_evaluator,
            policy_cell,
        })
    }
}

fn install_policy(
    config: &WorkforceConfig,
    credentials: &Credentials,
    toolbox: &Toolbox,
    admin: &Admin,
    skills: &Skills,
) -> Result<(Arc<PolicyEngine>, crate::wasm_tool_invoker::AdapterLoader)> {
    let Credentials {
        vault,
        global_vault,
        ..
    } = credentials;
    let Toolbox { tools, .. } = toolbox;
    let Admin {
        identity_store_handle,
        ..
    } = admin;
    let Skills {
        skill_state,
        wasm_invoker_slot,
        policy_cell,
        ..
    } = skills;
    // Build policy from file or built-in default
    let policy = Arc::new(load_policy(config)?);
    let _ = policy_cell.set(Arc::clone(&policy));

    // Fill this agent's `wf_tool_invoke` bridge slot — deferred to here
    // because the invoker needs the policy engine. Per-agent: each agent's
    // skill tools dispatch through that agent's own tool registry, so a
    // skill invoked via agent B never reaches tools registered only for
    // agent A.
    // Native adapters register lazily; a skill granted one of their tools
    // loads the adapter itself on first call, as a workflow step would.
    let adapter_loader: crate::wasm_tool_invoker::AdapterLoader = {
        let tools = Arc::clone(tools);
        let vault = vault.clone();
        let global_vault = global_vault.clone();
        let config = config.clone();
        let skill_state = Arc::clone(skill_state);
        Arc::new(move |family: &str| {
            if crate::skill_loader::skill_tools_registered(family, &tools) {
                return Ok(());
            }
            let Some(vault) = vault.as_ref() else {
                eyre::bail!("no vault to load {family} from");
            };
            let result = crate::skill_loader::load_and_track(
                family,
                Arc::clone(vault),
                global_vault.clone(),
                &config,
                &tools,
                &skill_state,
                "wasm-skill",
            )?;
            info!(skill = %family, tools = ?result.tool_names, "Loaded adapter for a WASM skill's tool call");
            Ok(())
        })
    };
    *wasm_invoker_slot.write() = Some(Arc::new(
        crate::wasm_tool_invoker::HubSkillToolInvoker::new(
            Arc::clone(tools),
            Arc::clone(&policy),
            Arc::clone(identity_store_handle),
            crate::authz::capability_matrix_handle(),
            tokio::runtime::Handle::current(),
        )
        .with_adapter_loader(Arc::clone(&adapter_loader)),
    ));
    Ok((policy, adapter_loader))
}

async fn register_telemetry_tools(
    config: &WorkforceConfig,
    setup: &Setup,
    credentials: &Credentials,
    toolbox: &Toolbox,
    skills: &Skills,
    vaults: &Vaults,
    config_path: &Option<PathBuf>,
    lock_manager: &Arc<LockManager>,
) {
    let Setup {
        agent_id,
        workspace,
        effective_llm,
        provider_kind_val,
        ..
    } = setup;
    let provider_kind_val = *provider_kind_val;
    let Credentials { providers, .. } = credentials;
    let Toolbox { tools, .. } = toolbox;
    let Skills {
        telemetry_store,
        cost_tracker,
        ..
    } = skills;
    let Vaults { agent_registry, .. } = vaults;
    tools.register(Arc::new(crate::relationship::RelationshipStatusTool::new(
        Arc::clone(telemetry_store),
        agent_id.clone(),
    )));
    // Load pricing from embedded TOML, then user overrides, then persisted DB
    let user_pricing_toml = workforce_common::config_files::read(&workspace.join("pricing.toml"))
        .ok()
        .flatten();
    cost_tracker.load_pricing_toml(
        workforce_agent::llm::DEFAULT_PRICING_TOML,
        user_pricing_toml.as_deref(),
    );
    // Layer the vision-specific pricing on top so vision-only models
    // (Qwen-VL, GLM-V) get costed via the same path as primary calls.
    if let Err(e) =
        cost_tracker.merge_pricing_toml_str(workforce_agent::llm::DEFAULT_VISION_PRICING_TOML)
    {
        tracing::warn!(error = %e, "Failed to merge default vision pricing");
    }
    // Layer transcription pricing too. Whisper-style APIs bill per
    // audio-minute, not per token; the file uses a per-millisecond-as-token
    // convention so transcription cost flows through the same path.
    if let Err(e) = cost_tracker
        .merge_pricing_toml_str(workforce_agent::llm::DEFAULT_TRANSCRIPTION_PRICING_TOML)
    {
        tracing::warn!(error = %e, "Failed to merge default transcription pricing");
    }
    cost_tracker.load_pricing_from_store();

    // Fetch pricing from OpenRouter API if enabled
    if effective_llm.fetch_pricing {
        let or_key = if provider_kind_val == ProviderKind::OpenRouter {
            effective_llm
                .api_key
                .clone()
                .or_else(|| providers.metered_key(ProviderKind::OpenRouter))
        } else {
            std::env::var("OPENROUTER_API_KEY").ok()
        };
        if let Some(ref key) = or_key {
            let store_ref = Arc::clone(telemetry_store);
            let tracker_ref = Arc::clone(cost_tracker);
            let key_owned = key.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::pricing_fetch::fetch_openrouter_pricing(
                    &key_owned,
                    &store_ref,
                    &tracker_ref,
                )
                .await
                {
                    tracing::warn!(error = %e, "Failed to fetch OpenRouter pricing");
                }
            });
        }
    }

    // Register usage statistics tools
    let usage_ctx = crate::usage_tools::UsageToolContext {
        store: Arc::clone(telemetry_store),
        agent_id: agent_id.as_str().to_string(),
        allowed_models: {
            let mut models = effective_llm.failover.clone();
            models.insert(0, effective_llm.default_model.clone());
            models
        },
    };
    for tool in crate::usage_tools::usage_tools(usage_ctx) {
        tools.register(tool);
    }

    // Register cost analytics tools (DW-340)
    let cost_analytics_ctx = crate::cost_analytics_tools::CostAnalyticsToolContext {
        store: Arc::clone(telemetry_store),
        agent_id: agent_id.as_str().to_string(),
    };
    for tool in crate::cost_analytics_tools::cost_analytics_tools(cost_analytics_ctx) {
        tools.register(tool);
    }

    // Register activity history tool (DW-365), bound to this agent so
    // queries default to its own activity, not the whole fleet's.
    tools.register(Arc::new(crate::activity_tool::ActivityTool::new(
        Arc::clone(telemetry_store),
        agent_id.as_str(),
    )));

    // Register pricing management tools (DW-334)
    let pricing_ctx = crate::pricing_tools::PricingToolContext {
        store: Arc::clone(telemetry_store),
        cost_tracker: Arc::clone(cost_tracker),
        provider: effective_llm.provider.clone(),
    };
    for tool in crate::pricing_tools::pricing_tools(pricing_ctx) {
        tools.register(tool);
    }

    // Register policy proposal tools
    let policy_tool_ctx = crate::policy_tool::PolicyToolContext {
        store: Arc::clone(telemetry_store),
        agent_id: agent_id.as_str().to_string(),
    };
    for tool in crate::policy_tool::policy_tools(policy_tool_ctx) {
        tools.register(tool);
    }

    // Role management tools
    let role_ctx = crate::role_tool::RoleToolContext {
        data_dir: config.hub.data_dir.clone(),
    };
    for tool in crate::role_tool::role_tools(role_ctx) {
        tools.register(tool);
    }

    // Agent management tools
    let agent_tool_ctx = crate::agent_tool::AgentToolContext {
        config_path: config_path.clone(),
        config: config.clone(),
        data_dir: config.hub.data_dir.clone(),
        lock_manager: Arc::clone(lock_manager),
        agent_id: agent_id.as_str().to_string(),
        def_store: shared_workflow_store(config).await,
        registry: Arc::clone(agent_registry),
    };
    for tool in crate::agent_tool::agent_tools(agent_tool_ctx) {
        tools.register(tool);
    }
}

fn register_heartbeat_tools(
    config: &WorkforceConfig,
    setup: &Setup,
    toolbox: &Toolbox,
) -> Option<Arc<crate::hub_core::heartbeat_signature::CheckSigner>> {
    let Setup {
        agent_id,
        workspace,
        ..
    } = setup;
    let Toolbox { tools, .. } = toolbox;
    // Heartbeat management tools
    let check_signer =
        crate::hub_core::heartbeat_signature::CheckSigner::from_config(config, agent_id.as_str())
            .map(Arc::new);
    if check_signer.is_none() {
        warn!(
            agent = %agent_id,
            "No vault key to sign heartbeat checks with: every check runs as the \
             heartbeat sentinel"
        );
    }
    let heartbeat_tool_ctx = crate::heartbeat_tool::HeartbeatToolContext {
        workspace: workspace.clone(),
        signer: check_signer.clone(),
        wake_handle: Arc::new(parking_lot::RwLock::new(None)),
        min_cooldown_secs: if config.budget.enabled {
            config.budget.min_recurring_interval_secs
        } else {
            0
        },
    };
    for tool in crate::heartbeat_tool::heartbeat_tools(heartbeat_tool_ctx) {
        tools.register(tool);
    }
    info!("Heartbeat management tools registered");
    check_signer
}

impl JobRunning {
    fn start(
        setup: &Setup,
        toolbox: &Toolbox,
        stores: &Stores,
        admin: &Admin,
        skills: &Skills,
        surface_registry: &Arc<tokio::sync::RwLock<SurfaceRegistry>>,
        memory_metrics: &mut crate::memory_metrics::MemoryMetrics,
    ) -> Self {
        let Setup {
            agent_id,
            workspace,
            ..
        } = setup;
        let Toolbox { tools, .. } = toolbox;
        let Stores { leader_gate, .. } = stores;
        let Admin { is_admin, .. } = admin;
        let Skills {
            telemetry_store, ..
        } = skills;
        // ─── Async Job Runner ──────────────────────────────────────────────
        let active_runners_for_jobs: Arc<tokio::sync::RwLock<HashMap<String, ActiveRunnerHandle>>> =
            Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        let capability_gate = Arc::new(parking_lot::RwLock::new(None));
        let job_runner: Option<Arc<dyn crate::hosts::JobHost>> = {
            let jobs_dir = workspace.join(".local").join("jobs");
            let persistence = crate::job_runner::JobPersistence::new(jobs_dir);
            let tool_executor: Arc<dyn crate::job_runner::ToolExecutor> =
                Arc::new(RegistryToolExecutor {
                    registry: Arc::clone(tools),
                    agent_id: agent_id.as_str().to_string(),
                    capability_gate: Arc::clone(&capability_gate),
                });

            // Stats recorder — feeds job metrics into the telemetry store.
            let stats_recorder: Arc<dyn crate::job_runner::StatsRecorder> =
                Arc::new(JobStatsRecorder {
                    telemetry_store: Arc::clone(telemetry_store),
                    agent_id: agent_id.as_str().to_string(),
                });

            // Session injector — pushes completion messages into active agent sessions.
            let session_injector: Arc<dyn crate::job_runner::SessionInjector> =
                Arc::new(JobSessionInjector {
                    active_runners: Arc::clone(&active_runners_for_jobs),
                });

            // Notification sink — announces completions to Slack for jobs that
            // set a `notify_channel` (e.g. sub-agents announcing to their thread).
            let notification_sink: Arc<dyn crate::job_runner::NotificationSink> =
                Arc::new(SurfaceNotificationSink {
                    surface_registry: Arc::clone(surface_registry),
                });

            let runner = crate::job_runner::JobRunner::new(
                agent_id.as_str().to_string(),
                persistence,
                None, // JetStream publisher wired later if NATS is available
                Some(notification_sink),
                Some(stats_recorder),
                Some(session_injector),
                tool_executor,
                crate::job_runner::DEFAULT_CONCURRENT_LIMIT,
            );
            memory_metrics.add_job_runner(agent_id.as_str(), Arc::new(runner.clone()));

            // Run startup recovery in background. Clustered: wait out the
            // deploy overlap and only recover on the leader — recovery marks
            // every `Running` manifest failed, which must never hit jobs still
            // executing on the instance that is draining next door.
            let recovery_runner = runner.clone();
            let recovery_gate = Arc::clone(leader_gate);
            tokio::spawn(async move {
                if !recovery_gate.wait_boot_singleton().await {
                    info!("Job runner startup recovery skipped — not the leader");
                    return;
                }
                if let Err(e) = recovery_runner.recover().await {
                    warn!(error = %e, "Job runner startup recovery failed");
                }
            });

            let jobs_tool =
                crate::jobs_tool::JobsTool::new(runner.clone(), agent_id.as_str().to_string())
                    .with_admin_policy(Arc::clone(is_admin));
            tools.register(Arc::new(jobs_tool));
            info!("Async job runner and jobs tool registered");
            Some(Arc::new(runner))
        };
        Self {
            active_runners: active_runners_for_jobs,
            capability_gate,
            job_runner,
        }
    }
}

fn build_trace_writer(
    config: &WorkforceConfig,
    setup: &Setup,
) -> Option<Arc<dyn workforce_agent::trace::TraceWriter>> {
    let Setup { agent_id, .. } = setup;
    match crate::trace_writer::resolve_traces_dir(config) {
        Some(traces_dir) => {
            match crate::trace_writer::JsonlTraceWriter::new(&traces_dir, agent_id.as_str()) {
                Ok(tw) => Some(Arc::new(tw) as Arc<dyn workforce_agent::trace::TraceWriter>),
                Err(e) => {
                    warn!(error = %e, "Failed to create trace writer — tracing disabled");
                    None
                }
            }
        }
        None => None,
    }
}

impl Media {
    fn build(
        config: &WorkforceConfig,
        setup: &Setup,
        credentials: &Credentials,
        toolbox: &Toolbox,
        stores: &Stores,
        skills: &Skills,
        surface_registry: &Arc<tokio::sync::RwLock<SurfaceRegistry>>,
    ) -> Self {
        let Setup {
            agent_id,
            workspace,
            effective_llm,
            ..
        } = setup;
        let Credentials {
            vault,
            global_vault,
            providers,
            ..
        } = credentials;
        let Toolbox { tools, .. } = toolbox;
        let Stores {
            memory_store,
            training_store,
            citation_store,
            ..
        } = stores;
        let Skills { cost_tracker, .. } = skills;
        let vision_tiers = load_vision_tier_registry(workspace);
        let describer = build_agent_describer(
            effective_llm,
            &vision_tiers,
            cost_tracker,
            agent_id.as_str(),
            providers.plans(),
            vault.clone(),
            global_vault.clone(),
        );

        if let Some(ref training) = training_store {
            if config.training.enabled {
                let mut training_add_tool = crate::training_tool::TrainingAddTool::new(
                    Arc::clone(training),
                    Arc::clone(memory_store),
                    Arc::clone(surface_registry),
                    config.training.clone(),
                    config.hub.data_dir.clone(),
                );
                if let Describer::Ready(llm, cfg) = &describer {
                    training_add_tool = training_add_tool.with_vision(Arc::clone(llm), cfg.clone());
                }
                tools.register(Arc::new(training_add_tool));
                tools.register(Arc::new(
                    crate::training_citations_tool::TrainingCitationsTool::new(
                        citation_store.clone(),
                    ),
                ));
            }
        }

        let transcription_tiers = load_transcription_tier_registry(workspace);
        let transcriber = build_agent_transcriber(
            effective_llm,
            &transcription_tiers,
            cost_tracker,
            agent_id.as_str(),
            vault.clone(),
            global_vault.clone(),
        );
        Self {
            vision_tiers,
            describer,
            transcription_tiers,
            transcriber,
        }
    }
}

/// 1. Explicit `hub.policy_path` in config
/// 2. `policies/default.yaml` if it exists (convention)
/// 3. Built-in allow-all fallback
pub fn load_policy(config: &WorkforceConfig) -> Result<PolicyEngine> {
    let base = if let Some(ref path) = config.hub.policy_path {
        info!(path, "Loading policy from configured path");
        let pc = PolicyConfig::from_file(path)?;
        PolicyEngine::new(pc)
    } else if std::path::Path::new("policies/default.yaml").exists() {
        info!(
            path = "policies/default.yaml",
            "Loading policy from default location"
        );
        let pc = PolicyConfig::from_file("policies/default.yaml")?;
        PolicyEngine::new(pc)
    } else {
        info!("No policy file found, using built-in allow-all");
        PolicyEngine::from_yaml(BUILTIN_ALLOW_ALL)?
    };
    Ok(base.with_constitutional_rules(builtin_constitutional_rules(config)))
}

/// Tool-specific defaults applied on top of any operator-supplied
/// policy. `projects.*` / `knowledge.refresh_index` are `allow` (gating
/// routine ops harms automation). `aws.s3.{list,head,get}` default to
/// `allow` — admin access control is the `[aws_s3].bucket_allowlist` (plus
/// the assumed role's IAM scope), not a per-call human prompt; operators
/// opt a sensitive bucket back into prompting via
/// `[aws_s3].require_approval_buckets`.
///
/// Constitutional rules are evaluated BEFORE the operator's YAML and the
/// engine is first-match, so a YAML rule on one of these exact tool names is
/// shadowed — operator tightening for them lives in config
/// (`[aws_s3]`, `[budget]`) and in the permissions cascade, which still
/// applies after this engine.
pub(super) fn builtin_constitutional_rules(
    config: &WorkforceConfig,
) -> Vec<workforce_policy::rules::PolicyRule> {
    let mut rules = crate::projects::tools::default_policies();
    rules.push(crate::knowledge::refresh_tool::default_policy());
    rules.extend(crate::aws::default_policies(&config.aws_s3));
    rules.extend(crate::aws::infra::default_policies());
    // Always present (not gated on `worker_read_sources`): the policy engine is
    // built at boot and not rebuilt on hot-reload, so gating it would block a
    // per-agent hot-enable. Harmless when off — `read_source.*` only exist in a
    // worker snapshot when the (live) flag enables their registration.
    rules.extend(crate::worker_read::default_policies());
    // The only write tool in the curation registry. Always present (the policy
    // engine is built at boot, not rebuilt on hot-reload); harmless when
    // self-learning is off — the tool only exists inside a curation snapshot
    // and self-gates on the `SELF_LEARNING_CURATION` scope.
    rules.extend(crate::self_learning::record::default_policies());
    rules.extend(crate::self_learning::record_node::default_policies());
    rules.extend(crate::hub_core::cost_governor::default_policies(
        &config.budget,
    ));
    rules.extend(crate::environment_tools::default_policies());
    rules
}

/// The artifact indexer `config` names: on vellev when `[artifacts]
/// index_backend` says so, wherever memories are kept, otherwise on
/// Weaviate when it is enabled, otherwise none (`Ok(None)`), and search
/// goes by keyword.
///
/// A vellev collection that cannot be used yet still gets its indexer: a
/// node not answering is made sure of on first use, and one an operator
/// has to put right (refused shape, key, tenant or model) is said loudly
/// here and looked at again every minute, so that it is used once put
/// right, without a restart. Meanwhile the store notes what the index did
/// not take, and search goes by keyword. `Err` only when no indexer can
/// be built at all, as with no `[vellev] api_key`.
pub(super) async fn artifact_indexer(
    config: &WorkforceConfig,
) -> eyre::Result<Option<Arc<dyn workforce_orchestrator::artifact_store::ArtifactIndexer>>> {
    use crate::artifact_index::{ArtifactIndex, VellevArtifacts, WeaviateArtifacts};
    if config.artifacts_indexed_in_vellev() {
        let v = &config.vellev;
        let key = v.api_key.as_deref().ok_or_else(|| {
            eyre::eyre!("[vellev] api_key is not set, so artifacts cannot be indexed in vellev")
        })?;
        let artifacts = VellevArtifacts::new(&v.url, key, &v.tenant)?;
        match artifacts.usable().await {
            Ok(()) => info!("Artifact vellev indexer initialised"),
            Err(e) => {
                error!(error = ?e, "The artifact index in vellev cannot be used: artifacts are found by keyword only until it is put right, and it is looked at again every minute")
            }
        }
        return Ok(Some(Arc::new(ArtifactIndex::new(artifacts))));
    }
    if !config.weaviate.enabled {
        return Ok(None);
    }
    let artifacts =
        WeaviateArtifacts::new(&config.weaviate.url, config.weaviate.api_key.as_deref());
    if let Err(e) = artifacts.ready().await {
        warn!(error = ?e, "Weaviate's Artifacts class is not usable yet; artifacts are indexed once it is");
    }
    info!("Artifact Weaviate indexer initialised");
    Ok(Some(Arc::new(ArtifactIndex::new(artifacts))))
}

pub async fn shared_artifact_store(
    config: &WorkforceConfig,
) -> Option<Arc<workforce_orchestrator::artifact_store::KnowledgeStore>> {
    crate::api::SHARED_ARTIFACT_STORE
        .get_or_init(|| async {
            let store = open_artifact_store(config).await?;
            // A new index holds nothing yet, and one may be lost or fall
            // behind later: keep it complete in the background, search
            // going by keyword whenever it is not.
            store.keep_index_complete(artifact_index_pace(config));
            open_artifact_mirror(config, &store).await;
            Some(store)
        })
        .await
        .clone()
}

/// What `workforce artifacts reindex` does: fill the artifact index
/// `config` names from Postgres, writing what it lacks or holds another
/// text of, and reporting `(done, of)` as it goes. For an index no hub
/// serves: it is refused, saying what to do, while one does (see
/// [`KnowledgeStore::seed_index`]). With `outside`, each vector is made
/// by that embedder rather than the vellev node, once a sample of the
/// artifacts' own texts shows it embeds as the node does; `checked` is told
/// what was measured. `Err`, saying why, when there is nothing to index
/// into or the index cannot be used; artifacts the index would not take
/// are in what it returns, and the index is then not marked.
///
/// With `mirror`, the vellev mirror of a shadowed artifact index is filled
/// instead (`workforce artifacts reindex --mirror`): refused unless
/// `[artifacts] index_backend = "shadow"`, and the serving index and its
/// pending list left alone.
///
/// [`KnowledgeStore::seed_index`]: workforce_orchestrator::artifact_store::KnowledgeStore::seed_index
pub async fn reindex_artifacts(
    config: &WorkforceConfig,
    mirror: bool,
    outside: Option<workforce_vellev::seeding::Seeding>,
    checked: &(dyn Fn(&workforce_vellev::seeding::Equivalence) + Send + Sync),
    progress: impl Fn(usize, usize),
) -> eyre::Result<workforce_orchestrator::artifact_store::Backfilled> {
    use workforce_orchestrator::artifact_store::KnowledgeStore;
    let pg = config
        .postgres
        .as_ref()
        .filter(|p| p.enabled)
        .ok_or_else(|| eyre::eyre!("no artifact store: [postgres] is not enabled"))?;
    let pool = KnowledgeStore::build_pool(pg)
        .map_err(|e| eyre::eyre!("no artifact store: Postgres cannot be reached: {e}"))?;
    reindex_artifacts_in(config, pool, mirror, outside, checked, progress).await
}

/// [`reindex_artifacts`], with the artifacts in `pool`.
pub(super) async fn reindex_artifacts_in(
    config: &WorkforceConfig,
    pool: workforce_common::postgres::PostgresPool,
    mirror: bool,
    outside: Option<workforce_vellev::seeding::Seeding>,
    checked: &(dyn Fn(&workforce_vellev::seeding::Equivalence) + Send + Sync),
    progress: impl Fn(usize, usize),
) -> eyre::Result<workforce_orchestrator::artifact_store::Backfilled> {
    use crate::artifact_index::{ArtifactIndex, VellevArtifacts};
    use workforce_orchestrator::artifact_store::{ArtifactIndexer, KnowledgeStore, MirrorStore};
    if mirror && config.artifacts.index_backend != workforce_common::settings::SHADOWED {
        eyre::bail!(
            "--mirror fills the vellev mirror of a shadowed artifact index, and [artifacts] index_backend is \"{}\": set it to \"shadow\" to keep a mirror, or leave out --mirror to fill the index it names",
            config.artifacts.index_backend
        );
    }
    let indexer: Arc<dyn ArtifactIndexer> = match outside {
        Some(seeding) => {
            if !mirror && !config.artifacts_indexed_in_vellev() {
                eyre::bail!(
                    "an outside embedder makes vectors for vellev, and artifacts are indexed in \"{}\" ([artifacts] index_backend)",
                    config.artifacts.index_backend
                );
            }
            let v = &config.vellev;
            let key = v
                .api_key
                .as_deref()
                .ok_or_else(|| eyre::eyre!("[vellev] api_key is not set"))?;
            let plain = VellevArtifacts::new(&v.url, key, &v.tenant)?;
            plain.ensure_collection().await?;
            let conn = pool.get().await?;
            workforce_common::telemetry_postgres::run_telemetry_migrations(&pool).await?;
            // What is embedded of an artifact, exactly: its title, tags
            // and content as the index writes them.
            let rows = conn
                .query(
                    "SELECT title, tags, content FROM artifacts ORDER BY random() LIMIT 200",
                    &[],
                )
                .await?;
            let texts: Vec<String> = rows
                .iter()
                .map(|row| {
                    let tags: Vec<String> =
                        serde_json::from_str(row.get::<_, &str>(1)).unwrap_or_default();
                    crate::artifact_index::text_embedded(row.get(0), &tags, row.get(2))
                })
                .collect();
            {
                let (tenants, embedding) = plain.embedded_as()?;
                let measured =
                    crate::seeding::artifacts_trusted(tenants, &embedding, &seeding, &texts)
                        .await?;
                checked(&measured);
            }
            let seeded = VellevArtifacts::new(&v.url, key, &v.tenant)?.seeded_by(seeding);
            Arc::new(ArtifactIndex::new(seeded))
        }
        None if mirror => mirror_indexer(config)?,
        None => artifact_indexer(config).await?.ok_or_else(|| {
            eyre::eyre!(
                "no artifact index is configured: neither [artifacts] index_backend = \"vellev\" nor [weaviate] enabled; artifacts are found by keyword only"
            )
        })?,
    };
    if mirror {
        return MirrorStore::seeding(pool, indexer)
            .await?
            .seed_index(progress)
            .await;
    }
    let store = KnowledgeStore::new(pool, Some(indexer)).await?;
    store.seed_index(progress).await
}

/// The indexer of the vellev mirror kept beside a shadowed artifact index.
fn mirror_indexer(
    config: &WorkforceConfig,
) -> eyre::Result<Arc<dyn workforce_orchestrator::artifact_store::ArtifactIndexer>> {
    use crate::artifact_index::{ArtifactIndex, VellevArtifacts};
    let v = &config.vellev;
    let key = v.api_key.as_deref().ok_or_else(|| {
        eyre::eyre!("[vellev] api_key is not set, so the artifact mirror cannot be kept in vellev")
    })?;
    Ok(Arc::new(ArtifactIndex::new(VellevArtifacts::new(
        &v.url, key, &v.tenant,
    )?)))
}

/// The vellev mirror kept beside the artifact index, when this process
/// opened one ([`open_artifact_mirror`]).
static MIRROR_ARTIFACT_STORE: std::sync::OnceLock<
    workforce_orchestrator::artifact_store::MirrorStore,
> = std::sync::OnceLock::new();

/// Keep the vellev mirror beside `store` when `config` shadows the
/// artifact index: noting for it turned on, its keeper started, and every
/// write through `store` waking it. When it does not, noting for a mirror
/// is turned off. A mirror that cannot be opened is said loudly and costs
/// the shadow, never the hub.
pub async fn open_artifact_mirror(
    config: &WorkforceConfig,
    store: &workforce_orchestrator::artifact_store::KnowledgeStore,
) {
    match artifact_mirror_for(config, store, || mirror_pool(config)).await {
        Ok(Some(mirror)) => {
            mirror.keep_index_complete(mirror_index_pace());
            // Reads are shadowed too, unless the comparisons have nowhere
            // to go: the mirror is kept either way.
            match artifact_read_shadow(config).await {
                Ok((reader, shadow)) => {
                    // Shared with the known-item runs, whose items take
                    // its permits.
                    let _ = ARTIFACT_SHADOW.set(Arc::clone(&shadow));
                    if let Err(e) = store.shadow_reads(&mirror, reader, shadow) {
                        error!(error = ?e, "Artifact searches are not shadowed");
                    }
                }
                Err(e) => {
                    error!(error = ?e, "Artifact searches are not shadowed: the mirror is kept, and nothing is compared until this is put right")
                }
            }
            info!("Artifact index mirror (vellev) kept beside the serving index");
            let _ = MIRROR_ARTIFACT_STORE.set(mirror);
        }
        Ok(None) => {}
        Err(e) => {
            error!(error = ?e, "The artifact index mirror cannot be kept: shadow mode has no mirror for artifacts until this is put right")
        }
    }
}

/// The mirror `config` asks for beside `store`, its keeper not started:
/// `None`, noting for a mirror turned off, when the artifact index is not
/// shadowed.
pub(super) async fn artifact_mirror_for(
    config: &WorkforceConfig,
    store: &workforce_orchestrator::artifact_store::KnowledgeStore,
    pool: impl FnOnce() -> eyre::Result<workforce_common::postgres::PostgresPool>,
) -> eyre::Result<Option<workforce_orchestrator::artifact_store::MirrorStore>> {
    use workforce_orchestrator::artifact_store::LEASE_WATCHES;
    // A hub with no mirror leaves the lease alone: it lapses by itself once
    // no hub renews it, and no hub can stop another's mirror.
    if config.artifacts.index_backend != workforce_common::settings::SHADOWED {
        return Ok(None);
    }
    let lease = mirror_index_pace().watch * LEASE_WATCHES;
    Ok(Some(
        store
            .open_mirror(pool()?, mirror_indexer(config)?, lease)
            .await?,
    ))
}

/// The mirror keeper's own pool, over `[postgres]`, every statement and
/// lock wait bounded ([`KnowledgeStore::build_mirror_pool`]).
///
/// [`KnowledgeStore::build_mirror_pool`]: workforce_orchestrator::artifact_store::KnowledgeStore::build_mirror_pool
fn mirror_pool(config: &WorkforceConfig) -> eyre::Result<workforce_common::postgres::PostgresPool> {
    use workforce_orchestrator::artifact_store::{KnowledgeStore, MIRROR_STATEMENT_LIMIT};
    let pg = config
        .postgres
        .as_ref()
        .filter(|p| p.enabled)
        .ok_or_else(|| eyre::eyre!("the artifact mirror needs [postgres] enabled = true"))?;
    Ok(KnowledgeStore::build_mirror_pool(
        pg,
        MIRROR_STATEMENT_LIMIT,
    )?)
}

/// What shadows a hub's artifact searches: the mirror's index as shadow
/// reads ask it, one attempt each within the kernel's deadline (A7), and
/// the kernel, at `[vellev.shadow.artifacts]`'s settings, paused as
/// `[vellev.shadow] paused` says, writing through the hub's one sink.
async fn artifact_read_shadow(
    config: &WorkforceConfig,
) -> eyre::Result<(
    Arc<dyn workforce_orchestrator::artifact_store::ArtifactIndexer>,
    Arc<workforce_common::shadow::Shadow>,
)> {
    use crate::artifact_index::{ArtifactIndex, VellevArtifacts, VellevPace as Pace};
    use workforce_common::settings::ShadowedStore;
    let settings = config.vellev.shadow.settings(ShadowedStore::Artifacts)?;
    let v = &config.vellev;
    let key = v
        .api_key
        .as_deref()
        .ok_or_else(|| eyre::eyre!("[vellev] api_key is not set"))?;
    let reader =
        VellevArtifacts::paced(&v.url, key, &v.tenant, Pace::shadow_read(settings.deadline))?;
    let shadow = artifact_shadow_kernel(config, crate::shadow::sink(config).await?)?;
    Ok((Arc::new(ArtifactIndex::new(reader)), Arc::new(shadow)))
}

/// The kernel that shadows artifact searches: `[vellev.shadow.artifacts]`'s
/// settings, paused as `[vellev.shadow] paused` says, writing to `sink`.
pub(super) fn artifact_shadow_kernel(
    config: &WorkforceConfig,
    sink: Arc<dyn workforce_common::shadow::ShadowSink>,
) -> eyre::Result<workforce_common::shadow::Shadow> {
    use workforce_common::settings::ShadowedStore;
    let settings = config.vellev.shadow.settings(ShadowedStore::Artifacts)?;
    let shadow = workforce_common::shadow::Shadow::new("artifacts", settings, sink)?;
    shadow.set_enabled(!config.vellev.shadow.paused);
    Ok(shadow)
}

/// The kernel that shadows this hub's artifact searches, once made.
static ARTIFACT_SHADOW: std::sync::OnceLock<Arc<workforce_common::shadow::Shadow>> =
    std::sync::OnceLock::new();

/// The kernel that shadows this hub's artifact searches, once the mirror
/// is open and its reads shadowed: what the artifacts' known-item runs
/// take their permits from.
pub fn artifact_shadow() -> Option<Arc<workforce_common::shadow::Shadow>> {
    ARTIFACT_SHADOW.get().cloned()
}

/// Whether the artifact mirror is in step, as the read shadow judges it
/// (`search == "index"` and not found out of step): `false` with none.
pub fn artifact_mirror_in_step() -> bool {
    artifact_mirror_state()
        .is_some_and(|(state, _)| state.search == Some("index") && state.in_step != Some(false))
}

/// The mirror's state as its keeper last knew it, and its lease; `None`
/// with no mirror.
pub fn artifact_mirror_state() -> Option<(
    workforce_orchestrator::artifact_store::IndexState,
    workforce_orchestrator::artifact_store::LeaseState,
)> {
    MIRROR_ARTIFACT_STORE
        .get()
        .map(|mirror| (mirror.index_state(), mirror.lease()))
}

/// The pace the artifact mirror is kept at: vellev's try deadline, and a
/// watch of 30 s (A12), so that its lag stays bounded whatever wakes it.
/// Its lease is renewed every watch.
pub fn mirror_index_pace() -> workforce_orchestrator::artifact_store::IndexPace {
    use workforce_orchestrator::artifact_store::IndexPace;
    IndexPace {
        watch: std::time::Duration::from_secs(30),
        deadline: crate::artifact_index::VELLEV_TRY_DEADLINE,
        ..IndexPace::DEFAULT
    }
}

/// The pace the artifact index is kept at: the keeper's own, with the
/// time one try at one artifact may take set for the index `config`
/// names. A vellev node is given [`TRY_DEADLINE`]; Weaviate keeps the
/// keeper's 150 s, a try there taking several requests of at most 30 s.
///
/// [`TRY_DEADLINE`]: crate::artifact_index::VELLEV_TRY_DEADLINE
pub fn artifact_index_pace(
    config: &WorkforceConfig,
) -> workforce_orchestrator::artifact_store::IndexPace {
    use workforce_orchestrator::artifact_store::IndexPace;
    match config.artifacts_indexed_in_vellev() {
        true => IndexPace {
            deadline: crate::artifact_index::VELLEV_TRY_DEADLINE,
            ..IndexPace::DEFAULT
        },
        false => IndexPace::DEFAULT,
    }
}

/// How long an artifact search waits for the index, and how long the index
/// is then left alone: for vellev's index, `[artifacts]
/// index_search_timeout_ms` and [`SEARCH_REST`]; for Weaviate's, as on
/// every release before, no deadline of the hub's.
///
/// [`SEARCH_REST`]: workforce_orchestrator::artifact_store::SEARCH_REST
pub fn artifact_search_wait(
    config: &WorkforceConfig,
) -> Option<(std::time::Duration, std::time::Duration)> {
    config.artifacts_indexed_in_vellev().then(|| {
        (
            std::time::Duration::from_millis(config.artifacts.index_search_timeout_ms),
            workforce_orchestrator::artifact_store::SEARCH_REST,
        )
    })
}

/// The artifact store `config` names, with its indexer, opened afresh, for
/// the process-wide [`shared_artifact_store`]. An indexer that cannot be
/// built is said, and the store opened without one.
pub async fn open_artifact_store(
    config: &WorkforceConfig,
) -> Option<Arc<workforce_orchestrator::artifact_store::KnowledgeStore>> {
    use workforce_orchestrator::artifact_store::KnowledgeStore;
    let pg = config.postgres.as_ref().filter(|p| p.enabled)?;
    let pool = match KnowledgeStore::build_pool(pg) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = ?e, "Artifact KnowledgeStore pool init failed; file fallback");
            return None;
        }
    };
    // Vector indexer + auto-tagger (DW-551): vellev's when memories are kept
    // there, else Weaviate's when it is enabled. The tagger LLM is installed
    // later via `artifact_index::set_tagger_llm`.
    let indexer = artifact_indexer(config).await.unwrap_or_else(|e| {
        error!(error = ?e, "No artifact indexer; artifacts are found by keyword only");
        None
    });
    crate::api::note_store_backends(config);
    // A deadline for searches of vellev's index, which the hub asks through
    // a client that waits and tries again for minutes. Weaviate's is asked
    // as it always was, with no deadline of the hub's.
    let opened = match artifact_search_wait(config) {
        Some((waiting, rest)) => KnowledgeStore::waiting(pool, indexer, waiting, rest).await,
        None => KnowledgeStore::new(pool, indexer).await,
    };
    match opened {
        Ok(store) => {
            info!("Artifact KnowledgeStore (Postgres) initialised");
            // One-shot disk→DB migration of legacy per-agent artifacts
            // (DW-548). Idempotent; originals are left in place.
            seed_legacy_artifacts(&store, &std::path::PathBuf::from(&config.hub.data_dir)).await;
            Some(store)
        }
        Err(e) => {
            warn!(error = ?e, "Artifact KnowledgeStore init failed; file fallback");
            None
        }
    }
}

/// Migrate legacy on-disk artifacts (`{data_dir}/agents/<id>/artifacts/*.md`,
/// written by the old `SharedWorkspace`) into the DB knowledge base. Owner is
/// the agent that held the workspace; visibility is `team`. Idempotent (skips
/// ids already present), non-fatal, leaves the files in place (DW-548).
pub(super) async fn seed_legacy_artifacts(
    store: &workforce_orchestrator::artifact_store::KnowledgeStore,
    data_dir: &std::path::Path,
) {
    use workforce_orchestrator::artifact_store::{Artifact, Visibility};
    use workforce_orchestrator::workspace::SharedWorkspace;

    let agents_dir = data_dir.join("agents");
    let Ok(rd) = std::fs::read_dir(&agents_dir) else {
        return;
    };
    let mut seeded = 0usize;
    for entry in rd.flatten() {
        let agent_path = entry.path();
        if !agent_path.is_dir() {
            continue;
        }
        if !agent_path.join("artifacts").is_dir() {
            continue;
        }
        let owner = entry.file_name().to_string_lossy().to_string();
        let Ok(ws) = SharedWorkspace::create(&agent_path) else {
            continue;
        };
        let Ok(olds) = ws.list_artifacts() else {
            continue;
        };
        for old in olds {
            let id = old.id.clone();
            let mut a = Artifact::new(
                old.id,
                old.artifact_type,
                old.title,
                old.content,
                owner.clone(),
                old.created_by.as_str(),
                Visibility::Team,
                vec![],
            );
            a.status = old.status;
            a.version = old.version;
            a.last_modified_by = old.last_modified_by.as_str().to_string();
            a.created_at = old.created_at;
            a.updated_at = old.updated_at;
            match store.seed(&a).await {
                Ok(true) => seeded += 1,
                Ok(false) => {}
                Err(e) => warn!(error = ?e, id = %id, "artifact migration: seed failed"),
            }
        }
    }
    if seeded > 0 {
        info!(seeded, "Migrated legacy disk artifacts into DB");
    }
}

/// Startup out-of-band sweep for identity-file revisions
///. For every
/// agent's human-editable identity files, compare the on-disk content with
/// the newest `def_revisions` row: a first-ever capture lands as the `seed`
/// baseline, later mismatches as unattributed `oob` revisions (agent
/// self-edits and container edits — coverage, not intrusion detection).
/// The write-through capture in the identity PUT handler keeps history
/// current between restarts; this sweep closes the gap for writes the hub
/// never intercepted.
pub async fn sweep_identity_revisions(
    store: &workforce_orchestrator::workflow_store::WorkflowStore,
    registry: &crate::agent_registry::AgentRegistry,
) {
    use workforce_orchestrator::workflow_store as ws;
    // Mirror of the gateway's ALLOWED_IDENTITY_FILES (the UI-editable set);
    // agent-managed memory files are deliberately out of scope — passive
    // capture churns them constantly and would flood the history.
    const SWEPT_IDENTITY_FILES: &[&str] = &[
        "SOUL.md",
        "AGENTS.md",
        "TOOLS.md",
        "IDENTITY.md",
        "WELCOME.md",
    ];

    /// Capture one file's on-disk state when it drifted past the newest
    /// revision. Returns `true` when a revision was recorded.
    async fn capture_oob(
        store: &workforce_orchestrator::workflow_store::WorkflowStore,
        kind: &str,
        ref_id: &str,
        content: &str,
    ) -> bool {
        use workforce_orchestrator::workflow_store as ws;
        let latest = match store.latest_revision_hash(kind, ref_id, None).await {
            Ok(h) => h,
            Err(e) => {
                warn!(error = ?e, ref_id = %ref_id, "revision sweep: hash lookup failed");
                return false;
            }
        };
        if latest.as_deref() == Some(ws::content_hash(content).as_str()) {
            return false;
        }
        let via = if latest.is_none() { "seed" } else { "oob" };
        match store
            .record_revision(
                kind,
                ref_id,
                None,
                content,
                false,
                ws::EditContext::new(via, None),
                None,
            )
            .await
        {
            Ok(recorded) => recorded.is_some(),
            Err(e) => {
                warn!(error = ?e, ref_id = %ref_id, "revision sweep: capture failed");
                false
            }
        }
    }

    let mut captured = 0usize;
    for service in registry.all_services() {
        for &filename in SWEPT_IDENTITY_FILES {
            let path = service.workspace.join(filename);
            let Ok(Some(content)) = workforce_common::config_files::read(&path) else {
                continue;
            };
            let ref_id = format!("{}/{}", service.agent_id, filename);
            if capture_oob(store, ws::KIND_IDENTITY_FILE, &ref_id, &content).await {
                captured += 1;
            }
        }
        // The workspace config.toml gets the same coverage: tool-driven
        // writes (system.set_routing / set_model persistence) and hand
        // edits coalesce into one unattributed revision per sweep.
        let config_path = service
            .workspace
            .join(workforce_common::config_files::WORKSPACE_CONFIG);
        if let Ok(Some(content)) = workforce_common::config_files::read(&config_path) {
            let ref_id = service.agent_id.to_string();
            if capture_oob(store, ws::KIND_AGENT_CONFIG, &ref_id, &content).await {
                captured += 1;
            }
        }
    }
    if captured > 0 {
        info!(
            captured,
            "Revision sweep captured out-of-band identity/config changes"
        );
    }
}

/// Create a webhook [`Router`](axum::Router) backed by the given agent service.
///
/// Returns the router ready to be `.merge()`-ed into the main application.
///
/// ```rust,ignore
/// let webhook_router = create_webhook_router(Arc::clone(&service), runtime);
/// let app = app.merge(webhook_router);
/// ```
pub fn create_webhook_router(
    service: Arc<AgentService>,
    workflow_runtime: Option<Arc<dyn crate::hosts::WorkflowHost>>,
) -> axum::Router {
    let state = std::sync::Arc::new(crate::webhooks::WebhookState {
        service,
        workflow_runtime,
    });
    crate::webhooks::router(state)
}

/// Create the HTTP trigger [`Router`](axum::Router) serving `POST /triggers/{path}`.
///
/// One wildcard route answers every addressing form: the minted
/// `t.{hex}` path and the owner-derived `{id}` / `{owner}/{id}` ones.
/// Returns the router ready to be `.merge()`-ed into the main application.
pub fn create_trigger_router(
    service: Arc<AgentService>,
    workflow_runtime: Arc<dyn crate::hosts::WorkflowHost>,
) -> axum::Router {
    let state = std::sync::Arc::new(crate::triggers::TriggerHttpState {
        service,
        runtime: workflow_runtime,
        oidc: std::sync::Arc::new(crate::trigger_oidc::OidcVerifier::new()),
    });
    crate::triggers::router(state)
}

/// Whether this agent offers what runs on a `kind` login: its
/// `[llm.logins]` has somewhere to hold one, and its live config routes
/// `kind` or someone has such a login stored where the policy reads.
/// Whose login pays varies by caller; one with none is told how to
/// connect.
pub(crate) fn offers_logins_of(
    service: &AgentService,
    kind: workforce_common::subscription::SubscriptionKind,
) -> bool {
    service.logins.may_hold_logins()
        && (routes_subscription(service, kind) || service.logins.stored_for_anyone(kind))
}

/// Whether the agent's config as it stands on disk routes a call to
/// `kind`'s subscription.
pub(crate) fn routes_subscription(
    service: &AgentService,
    kind: workforce_common::subscription::SubscriptionKind,
) -> bool {
    workforce_common::settings::AgentWorkspaceConfig::load(&service.workspace)
        .and_then(|workspace| service.admission.prospective(workspace.as_ref()))
        .inspect_err(|e| warn!(agent = %service.agent_id, error = %e, "config unreadable; its routes are not offered tools"))
        .is_ok_and(|prospective| {
            crate::subscription::routed_subscriptions(&prospective.effective, &prospective.next)
                .contains(&kind)
        })
}
