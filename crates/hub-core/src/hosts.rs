//! What `AgentService` holds its subsystems by. Each trait names exactly the
//! methods the rest of the hub calls on that subsystem through the service;
//! the subsystem implements it in its own module.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

use tokio_util::sync::CancellationToken;

use async_trait::async_trait;
use eyre::Result;
use serde::Serialize;
use uuid::Uuid;
use workforce_agent::compile_gate::CompileGate;
use workforce_common::channels::{ChannelSurface, SurfaceRegistry};
use workforce_common::persistence::WorkflowInstanceRow;
use workforce_common::persona::PersonaOverrides;
use workforce_common::session::{SessionAttachment, TurnPrincipal};
use workforce_common::settings::{AgentConfig, CompileGateConfig, OwnerType};
use workforce_common::subscription::{AgentOwner, SubscriptionKind};
use workforce_core::access::{TeamMembership, Visibility};
use workforce_core::policy::PermissionPromptOptions;
use workforce_core::types::AgentId;
use workforce_orchestrator::monitor::{MonitorDefinition, MonitorKey, MonitorStore};
use workforce_orchestrator::workflow::definition::{TriggerDefinition, WorkflowDefinition};
use workforce_orchestrator::workflow::instance::TriggerInfo;
use workforce_orchestrator::workflow_store::{DefRevision, EditContext, RunAsWrite, WorkflowStore};

use crate::def_write_auth::DefWriter;
use crate::hub_core::jobs::AgentExecutor;
use crate::hub_core::jobs::JobRunnerCacheStats;
use crate::hub_core::jobs::{
    ChainId, ChainStep, ChainSummary, JobId, JobManifest, JobOutput, JobReader, JobType,
};
use crate::hub_core::logins::resolve::{Login, NoLogin, Vaults};
use crate::hub_core::monitors::{MonitorSummary, Probe};
use crate::hub_core::pipelines::HubReads;
use crate::hub_core::playbooks::{PlaybookOutcome, PlaybookResolution, PlaybookSummary};
use crate::hub_core::schedules::{
    EditRefused, JobEdit, JobPayload, JobRunRecord, Outcomes, Schedule, ScheduledJob,
};
use crate::hub_core::workflows::{
    AutomatedRunBlock, ConsolidationTemplateSummary, DecisionAsk, DecisionMade, DecisionRefused,
    ResumeAsk, ResumeRefused, Resumed, TriggerUrls, WorkflowStreamEvent, WorkflowSummary,
};
use crate::resource_access::DefListing;
use crate::run_steering::{Steer, SteerAction, SteerRefused, Steerer};
use crate::serve::{AgentService, IsolatedRun, MessageProgress, RunAuthor, TurnInbox, TurnOutcome};
use crate::vault::Vault;

/// One installed MCP server's status for the environment state board — the
/// store-vs-registry join produced by [`McpHost::server_statuses`].
#[derive(Debug, Clone, Serialize)]
pub struct ServerStatus {
    pub server_id: String,
    /// Workspace group, if the server is grouped (`[mcp.servers.*].workspace`).
    pub workspace: Option<String>,
    /// Variant label within the workspace (`staging`/`prod`/…).
    pub env: Option<String>,
    /// In the live registry — initialised and its tools are callable.
    pub connected: bool,
    /// Installed but not connected, and OAuth-based — i.e. it most likely just
    /// needs `mcp.authorize <server_id>`. The signal that tells "needs auth"
    /// apart from "never installed".
    pub needs_auth: bool,
}

/// This agent's MCP servers, as the environment surfaces see them.
pub trait McpHost: Send + Sync {
    /// Every installed server with whether it is connected or needs auth.
    fn server_statuses(&self) -> Vec<ServerStatus>;
    /// Persist a workspace's default environment.
    fn set_workspace_default(&self, workspace: &str, default_env: &str) -> eyre::Result<()>;
}

/// Where this agent's build checks run, handed out per turn under the
/// agent's `[tools.compile_gate]` as of that turn.
pub trait CompileGateHost: Send + Sync {
    fn gate(self: Arc<Self>, config: CompileGateConfig) -> Arc<dyn CompileGate>;
}

/// The self-learning runner's wake-up: `run now` for one plan.
#[async_trait]
pub trait SelfLearningWake: Send + Sync {
    /// Whether the runner took the request.
    async fn wake_plan(&self, plan_id: String) -> bool;
}

/// A routed approval whose durable row exists and whose approvers still need
/// their Slack DMs — raised by a remote MCP turn or parked call, or by a CLI
/// round's MCP call. `channel` and `thread` are where the requester is told
/// who was asked; a remote call has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedApproval {
    pub agent_id: String,
    pub request_id: String,
    pub tool_name: String,
    pub detail: String,
    pub reason: String,
    pub options: PermissionPromptOptions,
    pub approver_user_ids: Vec<String>,
    pub requester_user_id: String,
    pub channel: Option<String>,
    pub thread: Option<String>,
}

impl RoutedApproval {
    /// What a caller is shown of an approval routed to people elsewhere.
    pub fn notice(&self) -> serde_json::Value {
        serde_json::json!({
            "request_id": self.request_id,
            "tool": self.tool_name,
            "approvers": self.approver_user_ids,
        })
    }
}

/// A configured agent that is not serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotServing {
    /// Being built, for the first time or again after a config change.
    Starting,
    /// Its build failed; why, with secrets redacted.
    Failed(String),
}

impl std::fmt::Display for NotServing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting => f.write_str("starting"),
            Self::Failed(reason) => write!(f, "failed to load: {reason}"),
        }
    }
}

/// A configured agent that is not serving, with the entry it is built from.
#[derive(Debug, Clone)]
pub struct Unloaded {
    pub config: AgentConfig,
    pub state: NotServing,
}

/// How an agent gets an LLM provider and switches its primary: what the
/// service calls on the providers composition builds for it.
#[async_trait::async_trait]
pub trait ProviderHost: Send + Sync {
    /// The agent's provider of `kind`, serving `model` by default: a
    /// subscription provider on the agent's logins, a metered provider on
    /// `api_key`, else on the agent's own key for `kind`.
    async fn provider(
        &self,
        kind: workforce_agent::llm::provider::ProviderKind,
        model: Option<&str>,
        thinking_budget: Option<u32>,
        zai_subscription: bool,
        api_key: Option<String>,
    ) -> eyre::Result<Arc<dyn workforce_agent::llm::provider::LlmProvider>>;

    /// Switch the agent to `provider`, of `kind`: it becomes the pool's
    /// default, and the chain's externals are those `llm` and `resolved`
    /// route to beyond the new primary.
    async fn swap_primary(
        &self,
        pool: Option<&workforce_agent::llm::pool::PooledProvider>,
        failover: Option<&workforce_agent::llm::failover::FailoverProvider>,
        kind: workforce_agent::llm::provider::ProviderKind,
        provider: Arc<dyn workforce_agent::llm::provider::LlmProvider>,
        llm: &workforce_common::settings::LlmConfig,
        resolved: &crate::hub_core::resolved_config::ResolvedConfig,
    );
}

/// The multi-agent registry as the agents see it, once it is set.
pub trait RegistryView: Send + Sync {
    fn get(&self, id: &str) -> Option<Arc<AgentService>>;
    fn default_agent(&self) -> Option<Arc<AgentService>>;
    fn agent_ids(&self) -> Vec<AgentId>;
    fn all_services(&self) -> Vec<Arc<AgentService>>;
    /// The configured agents that are not serving, by id.
    fn unloaded(&self) -> Vec<Unloaded>;
    /// Whether `identity` may use `agent` as a chat with it would decide.
    fn admits(&self, agent: &str, identity: &str) -> bool;
    /// Every vault a login may be stored in, across the registered agents.
    fn login_vaults(&self, global: Option<&Arc<Vault>>) -> Vec<Arc<Vault>>;
    /// Where approvals routed to Slack are handed off.
    fn routed_delivery(self: Arc<Self>) -> tokio::sync::mpsc::Sender<RoutedApproval>;
}

/// The registry handle a service is given once the registry exists; empty
/// until then, and in single-agent paths.
pub type RegistryCell = Arc<OnceLock<Weak<dyn RegistryView>>>;

/// [`RegistryView::admits`] through `registry` once it is set; nobody before.
pub fn admits_through(registry: RegistryCell) -> Arc<dyn Fn(&str, &str) -> bool + Send + Sync> {
    Arc::new(move |agent, identity| {
        registry
            .get()
            .and_then(Weak::upgrade)
            .is_some_and(|registry| registry.admits(agent, identity))
    })
}

/// This agent's background jobs, as the tools, surfaces and lifecycle code
/// drive them through the service.
#[async_trait]
pub trait JobHost: Send + Sync {
    async fn submit(&self, manifest: JobManifest) -> eyre::Result<JobId>;
    async fn submit_chain(
        &self,
        steps: Vec<ChainStep>,
        agent_id: String,
        notify_channel: Option<String>,
        notify_thread: Option<String>,
        session_channel: Option<String>,
    ) -> eyre::Result<(ChainId, Vec<(String, JobId)>)>;
    async fn get_result(
        &self,
        job_id: &str,
        tail: Option<usize>,
    ) -> eyre::Result<serde_json::Value>;
    async fn chain_summary(&self, chain_id: &str) -> Option<ChainSummary>;
    async fn job_detail_for(
        &self,
        job_id: &str,
        reader: &JobReader,
    ) -> Option<(JobManifest, Option<JobOutput>)>;
    async fn list_by_type(&self, job_type: JobType) -> Vec<JobManifest>;
    async fn readable_by(&self, job_id: &str, reader: &JobReader) -> bool;
    async fn cancel(&self, job_id: &str) -> eyre::Result<bool>;
    async fn cancel_chain(&self, chain_id: &str) -> eyre::Result<bool>;
    /// The jobs any of `names` submitted.
    async fn jobs_for(&self, names: &[String]) -> eyre::Result<Vec<JobManifest>>;
    /// The running jobs that act as any of `names`.
    async fn active_run_as(&self, names: &[String]) -> eyre::Result<Vec<JobManifest>>;
    /// How many jobs each person has submitted.
    async fn people(&self) -> eyre::Result<std::collections::BTreeMap<String, usize>>;
    /// Hand every job of `names` to `to`; how many moved.
    async fn reassign_person(&self, names: &[String], to: &str) -> eyre::Result<usize>;
    /// Drop every job of `names`; how many went, and the ids still running.
    async fn forget_person(&self, names: &[String]) -> eyre::Result<(usize, Vec<String>)>;
    fn set_agent_executor(&self, executor: Arc<dyn AgentExecutor>);
    async fn cache_stats(&self) -> JobRunnerCacheStats;
}

/// Which subscription login pays for this agent's calls, as the engines,
/// tools and surfaces ask through the service.
pub trait LoginHost: Send + Sync {
    fn agent_id(&self) -> &str;
    /// The login a call of `kind` runs on for the current payer.
    fn resolve(&self, kind: SubscriptionKind) -> Result<Login, NoLogin>;
    /// A stored login of `kind` that `accepts`, cooling or not.
    fn holding(&self, kind: SubscriptionKind, accepts: &dyn Fn(&Login) -> bool) -> Option<Login>;
    fn owner(&self) -> AgentOwner;
    fn effective_owner(&self) -> (Option<String>, OwnerType);
    fn vaults(&self) -> Vaults<'_>;
    /// Whether `person` may share logins on this agent.
    fn may_share(&self, person: Option<&str>, is_admin: bool) -> bool;
    /// Whether this agent's config has somewhere to hold logins.
    fn may_hold_logins(&self) -> bool;
    /// Whether anyone has a login of `kind` stored where the policy reads.
    fn stored_for_anyone(&self, kind: SubscriptionKind) -> bool;
}

/// A runtime's definitions, as the definition commands, the HTTP routes and
/// the identity lifecycle reach them: where a row lives, who owns it, and how
/// to make a change take effect.
#[async_trait]
pub trait DefinitionSurface: Send + Sync {
    fn def_store(&self) -> Option<&Arc<WorkflowStore>>;
    async fn definition_ownership(
        &self,
        bare_id: &str,
        owner: Option<&str>,
    ) -> Option<(Option<String>, Visibility)>;
    /// Every loaded definition's id with its owner.
    async fn owners(&self) -> Vec<(String, Option<String>)>;
    /// Re-read the definitions; how many loaded.
    async fn reload(&self) -> eyre::Result<usize>;
}

/// The playbook runtime as the surfaces, the prompt and the HTTP routes use it.
#[async_trait]
pub trait PlaybookHost: DefinitionSurface {
    async fn resolve_for_user(
        &self,
        bare_id: &str,
        user: &str,
        teams: &dyn TeamMembership,
    ) -> PlaybookResolution;
    async fn list_definitions_visible(&self, listing: &DefListing) -> Vec<PlaybookSummary>;
    /// Drive a playbook from a chat message, if the message is a directive.
    #[allow(clippy::too_many_arguments)]
    async fn handle_message(
        &self,
        svc: &Arc<AgentService>,
        session_key: &str,
        owner: &str,
        surfaces: &[Arc<dyn ChannelSurface>],
        channel: &str,
        thread_id: &Option<String>,
        text: &str,
    ) -> PlaybookOutcome;
    fn get_definition_source(&self, id: &str, owner: Option<&str>) -> eyre::Result<String>;
    async fn save_definition_source(
        &self,
        id: &str,
        toml_content: &str,
        owner: Option<&str>,
        visibility: Visibility,
        edit: EditContext<'_>,
    ) -> eyre::Result<()>;
    fn export_all_sources(&self) -> eyre::Result<HashMap<String, String>>;
    async fn delete_definition(
        &self,
        id: &str,
        owner: Option<&str>,
        edit: EditContext<'_>,
    ) -> eyre::Result<()>;
    async fn list_revisions(&self, id: &str, owner: Option<&str>)
        -> eyre::Result<Vec<DefRevision>>;
    async fn get_revision(
        &self,
        id: &str,
        owner: Option<&str>,
        rev: i32,
    ) -> eyre::Result<Option<DefRevision>>;
}

/// The monitor runtime's definitions, as the HTTP routes edit them.
#[async_trait]
pub trait MonitorDefinitions: DefinitionSurface {
    async fn list_definitions_visible(&self, listing: &DefListing) -> Vec<MonitorSummary>;
    async fn stored_definition(&self, id: &str, owner: Option<&str>) -> Option<MonitorDefinition>;
    async fn check_definition(&self, def: &MonitorDefinition) -> eyre::Result<()>;
    fn get_definition_source(&self, id: &str, owner: Option<&str>) -> eyre::Result<String>;
    async fn save_definition_source(
        &self,
        id: &str,
        toml_content: &str,
        owner: Option<&str>,
        visibility: Visibility,
        edit: EditContext<'_>,
    ) -> eyre::Result<()>;
    fn export_all_sources(&self) -> eyre::Result<HashMap<String, String>>;
    async fn delete_definition(
        &self,
        id: &str,
        owner: Option<&str>,
        edit: EditContext<'_>,
    ) -> eyre::Result<()>;
    async fn list_revisions(&self, id: &str, owner: Option<&str>)
        -> eyre::Result<Vec<DefRevision>>;
    async fn get_revision(
        &self,
        id: &str,
        owner: Option<&str>,
        rev: i32,
    ) -> eyre::Result<Option<DefRevision>>;
}

/// The channel-monitor engine as the surfaces, the scheduler, the definition
/// commands, the HTTP routes and its peer engines use it.
#[async_trait]
pub trait MonitorHost: Send + Sync {
    fn definitions(&self) -> &dyn MonitorDefinitions;
    fn store(&self) -> &Arc<MonitorStore>;
    fn surfaces(&self) -> &Arc<tokio::sync::RwLock<SurfaceRegistry>>;
    /// Teach the loop guard the ids this engine's own output carries on `surface`.
    async fn register_self(&self, surface: &str, ids: Vec<String>);
    async fn sweep_all(&self);
    fn personas(&self) -> Arc<PersonaOverrides>;
    async fn reconcile_personas(&self);
    /// Whether this engine could run `def`; why not, in the operator's terms.
    async fn preflight(&self, def: &MonitorDefinition) -> Result<(), String>;
    /// This engine's reading of `def`'s channel, for the operator.
    async fn probe(&self, def: &MonitorDefinition, owner: Option<&str>) -> Option<Probe>;
    async fn list_visible(&self, listing: &DefListing) -> Vec<MonitorSummary>;
    async fn summary_of(&self, id: &str, owner: Option<&str>) -> Option<MonitorSummary>;
    async fn delete_monitor(
        &self,
        id: &str,
        owner: Option<&str>,
        edit: EditContext<'_>,
    ) -> eyre::Result<()>;
    /// What a save or resume of `def` does about the events it missed.
    async fn apply_catch_up_policy(
        &self,
        key: &MonitorKey,
        def: &MonitorDefinition,
        previous: Option<&MonitorDefinition>,
    );
    /// Whether this engine could run `def`, for a peer deciding where it runs.
    async fn preflight_here(&self, def: &MonitorDefinition) -> Result<(), String>;
    /// This engine's reading of `def`'s channel, for a peer that does not watch it.
    async fn probe_here(&self, def: &MonitorDefinition, owner: Option<&str>) -> Option<Probe>;
}

/// This agent's scheduler, as the turn code, tools, surfaces, lifecycle and
/// supervisor drive it through the service.
#[async_trait]
pub trait SchedulerHost: Send + Sync {
    async fn set_messaging_context(
        &self,
        session_key: &str,
        channel_id: String,
        thread_id: Option<String>,
    );
    async fn get_messaging_context(&self, session_key: &str) -> Option<(String, Option<String>)>;
    async fn clear_messaging_context(&self, session_key: &str);
    async fn list_jobs(&self) -> Vec<ScheduledJob>;
    async fn get_job(&self, job_id: &str) -> Option<ScheduledJob>;
    fn job_spend(&self, job_id: &str) -> Option<f64>;
    async fn add_job_with_desc(
        &self,
        name: &str,
        description: Option<&str>,
        schedule: Schedule,
        payload: JobPayload,
    ) -> Result<String, String>;
    async fn add_job_owned(
        &self,
        name: &str,
        description: Option<&str>,
        schedule: Schedule,
        payload: JobPayload,
        creator_identity_id: Option<String>,
        visibility: Option<String>,
    ) -> Result<String, String>;
    async fn update_job(
        &self,
        job_id: &str,
        edit: JobEdit,
        editor: &str,
    ) -> Result<Option<ScheduledJob>, EditRefused>;
    async fn remove_job(&self, job_id: &str) -> bool;
    async fn remove_workflow_start_jobs(&self) -> usize;
    async fn run_job_now(&self, job_id: &str) -> Option<JobRunRecord>;
    async fn recent_outcomes(&self, job_id: &str, n: usize) -> Outcomes;
    async fn current_jobs(&self) -> Vec<ScheduledJob>;
    async fn forget_jobs(&self, ids: &[String]) -> usize;
    async fn release_jobs(&self, names: &[String]) -> Vec<String>;
    async fn hold_for_suspension(&self, names: &[String], apply: bool) -> Vec<ScheduledJob>;
    async fn reassign_jobs(
        &self,
        ids: Option<&[String]>,
        names: &[String],
        to: &str,
        editor: Option<&str>,
    ) -> Vec<String>;
    /// Point the Desk delivery of each job `owner` owns that `retarget`
    /// moves elsewhere; the ids changed.
    async fn retarget_desk(
        &self,
        owner: &str,
        retarget: &(dyn for<'t> Fn(
            &'t workforce_common::delivery::DeliveryTarget,
        ) -> Option<workforce_common::delivery::DeliveryTarget>
              + Send
              + Sync),
    ) -> Vec<String>;
    /// Release the jobs held for people `suspended` no longer says are.
    async fn release_suspension_holds(
        &self,
        suspended: &(dyn for<'p> Fn(&'p str) -> bool + Send + Sync),
        apply: bool,
    ) -> Vec<ScheduledJob>;
    /// The jobs that name any of `names` as owner, run-as or editor.
    async fn jobs_naming(&self, names: &[String]) -> Vec<ScheduledJob>;
    async fn upsert_job(
        &self,
        name: &str,
        description: Option<&str>,
        schedule: Schedule,
        payload: JobPayload,
    ) -> Result<String, String>;
    async fn update_job_run_as(&self, job_id: &str, identity_id: Option<String>) -> bool;
    async fn update_job_identity(&self, job_id: &str, identity_id: Option<String>) -> bool;
    async fn update_job_bill_to(&self, job_id: &str, identity_id: Option<String>) -> bool;
    async fn job_run_history(&self, job_id: &str) -> Vec<JobRunRecord>;
    /// Stop the tick loop for good.
    fn retire(&self);
    /// The tick loop, until `cancel`.
    async fn run(self: Arc<Self>, cancel: tokio_util::sync::CancellationToken);
}

/// What applies a re-read configuration to the service: validates it,
/// swaps what changed, and names what needs a restart.
#[async_trait]
pub trait ReconfigureHost: Send + Sync {
    async fn apply(&self, service: &AgentService) -> eyre::Result<Vec<String>>;
}

/// What runs this agent's turns, as the surfaces, tools, jobs and
/// schedules drive them through the service.
#[async_trait]
pub trait TurnHost: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn process_message(
        &self,
        service: &Arc<AgentService>,
        channel: &str,
        user: &str,
        message: &str,
        external_id: Option<&str>,
        channel_system_prompt: Option<&str>,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        attachments: Option<Vec<SessionAttachment>>,
        max_rounds_override: Option<u32>,
        model_override: Option<&str>,
        task_id: Option<&str>,
        trust_level: Option<workforce_common::trust::TrustLevel>,
        run_as: Option<&str>,
    ) -> Result<String>;
    #[allow(clippy::too_many_arguments)]
    async fn process_message_with_progress(
        &self,
        service: &Arc<AgentService>,
        channel: &str,
        user: &str,
        run_as: Option<&str>,
        message: &str,
        external_id: Option<&str>,
        session_id: Option<&str>,
        attachments: Option<Vec<SessionAttachment>>,
        channel_system_prompt: Option<&str>,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        inbox: Option<TurnInbox>,
        display_content: Option<&str>,
        ticket_id: Option<&str>,
        step_max_rounds: Option<u32>,
        step_model: Option<&str>,
        step_context_eviction_interval: Option<u32>,
        step_context_eviction_keep_last: Option<u32>,
        tool_whitelist: Option<&[String]>,
        persona_override: Option<&str>,
        workspace_id: Option<&str>,
        trust_level: Option<workforce_common::trust::TrustLevel>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
        voice_channel: Option<&str>,
        desk: Option<&workforce_common::channels::desk::DeskTurnContext>,
    ) -> Result<MessageProgress>;
    #[allow(clippy::too_many_arguments)]
    async fn process_turn(
        &self,
        service: &Arc<AgentService>,
        channel: &str,
        user: &str,
        message: &str,
        external_id: Option<&str>,
        channel_system_prompt: Option<&str>,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        attachments: Option<Vec<SessionAttachment>>,
        max_rounds_override: Option<u32>,
        model_override: Option<&str>,
        task_id: Option<&str>,
        trust_level: Option<workforce_common::trust::TrustLevel>,
        run_as: Option<&str>,
        author: RunAuthor,
    ) -> Result<TurnOutcome>;
    async fn run_isolated(
        &self,
        service: &Arc<AgentService>,
        channel: &str,
        task: &str,
        model_override: Option<&str>,
        system_prompt_override: Option<&str>,
        run: &IsolatedRun,
        profile: crate::hub_core::jobs::AgentProfile,
        curation: Option<(
            crate::hub_core::curation::CurationCtx,
            workforce_common::learning_plan::CurationMode,
        )>,
        task_label: Option<&str>,
    ) -> Result<crate::hub_core::jobs::AgentRunResult>;
    /// Whether a turn started now on `model`, else the agent's default, has a
    /// route to run on.
    async fn has_route(&self, service: &AgentService, model: Option<&str>) -> bool;
    async fn admit_turn_or_deny(
        &self,
        service: &AgentService,
        identity: Option<&str>,
        channel_id: Option<&str>,
    ) -> Option<String>;
}

/// The consolidation templates, as the routes and the definition commands
/// read and edit them.
#[async_trait]
pub trait ConsolidationHost: Send + Sync {
    async fn list_definitions_visible(
        &self,
        listing: &DefListing,
    ) -> eyre::Result<Vec<ConsolidationTemplateSummary>>;
    async fn definition_ownership(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> Option<(Option<String>, Visibility)>;
    async fn get_definition_source(&self, id: &str, owner: Option<&str>) -> eyre::Result<String>;
    async fn save_definition_source(
        &self,
        id: &str,
        source: &str,
        owner: Option<&str>,
        visibility: Visibility,
        edit: EditContext<'_>,
    ) -> eyre::Result<()>;
    async fn delete_definition(
        &self,
        id: &str,
        owner: Option<&str>,
        edit: EditContext<'_>,
    ) -> eyre::Result<()>;
    async fn list_revisions(&self, id: &str, owner: Option<&str>)
        -> eyre::Result<Vec<DefRevision>>;
    async fn get_revision(
        &self,
        id: &str,
        owner: Option<&str>,
        rev: i32,
    ) -> eyre::Result<Option<DefRevision>>;
}

/// This hub's workflow runtime, as the turn code, tools, surfaces, routes,
/// scheduler, monitors, lifecycle and supervisor drive it through the
/// service.
#[async_trait]
pub trait WorkflowHost: DefinitionSurface + HubReads {
    fn set_store(&self, store: Arc<WorkflowStore>);
    fn set_mcp_exposed_tools(&self, patterns: Vec<String>);
    fn set_agent_registry(&self, registry: Arc<dyn RegistryView>);
    fn set_agent_surfaces(
        &self,
        agent_bindings: &HashMap<String, String>,
        workspace_surfaces: &HashMap<String, Arc<dyn ChannelSurface>>,
    );
    fn set_shutdown_token(&self, token: CancellationToken);
    async fn reload_definitions(&self) -> eyre::Result<usize>;
    async fn sync_schedule_triggers(&self) -> eyre::Result<usize>;
    fn can_schedule(&self) -> bool;
    /// Resume the runs that were in flight when the hub last stopped.
    async fn recover_running_instances(self: Arc<Self>);
    async fn start_maintenance_loop(
        self: Arc<Self>,
        interval_secs: u64,
        drain_token: CancellationToken,
    );
    /// Resume the runs held for people no longer suspended.
    async fn resume_suspension_held(self: Arc<Self>);
    async fn list_definitions(&self) -> Vec<WorkflowSummary>;
    async fn list_definitions_visible(&self, listing: &DefListing) -> Vec<WorkflowSummary>;
    async fn get_definition(&self, id: &str) -> Option<WorkflowDefinition>;
    async fn all_definitions_keyed(&self) -> Vec<(String, WorkflowDefinition)>;
    async fn readable_definition(
        &self,
        key: &str,
        reader: &DefWriter,
    ) -> Option<WorkflowDefinition>;
    /// The agent each of `keys`' definitions declares, by key.
    async fn declared_agents(&self, keys: &[&str]) -> HashMap<String, String>;
    fn get_definition_source(&self, id: &str, owner: Option<&str>) -> eyre::Result<String>;
    async fn save_definition_source(
        &self,
        id: &str,
        toml_content: &str,
        owner: Option<&str>,
        visibility: Visibility,
        run_as: RunAsWrite,
        edit: EditContext<'_>,
    ) -> eyre::Result<Vec<String>>;
    async fn delete_definition(
        &self,
        id: &str,
        owner: Option<&str>,
        edit: EditContext<'_>,
    ) -> eyre::Result<()>;
    fn export_all_sources(&self) -> eyre::Result<HashMap<String, String>>;
    async fn list_revisions(&self, id: &str, owner: Option<&str>)
        -> eyre::Result<Vec<DefRevision>>;
    async fn get_revision(
        &self,
        id: &str,
        owner: Option<&str>,
        rev: i32,
    ) -> eyre::Result<Option<DefRevision>>;
    async fn definition_run_as(&self, bare_id: &str, owner: Option<&str>) -> Option<String>;
    async fn automated_run_block(
        &self,
        key: &str,
        run_identity: Option<&str>,
    ) -> Option<AutomatedRunBlock>;
    async fn annotate_automation(&self, summaries: Vec<WorkflowSummary>) -> Vec<WorkflowSummary>;
    async fn summaries_json(&self, summaries: &[WorkflowSummary]) -> Vec<serde_json::Value>;
    async fn with_trigger_urls(&self, def_key: &str, entry: serde_json::Value)
        -> serde_json::Value;
    async fn trigger_urls(&self, def_key: &str) -> Vec<TriggerUrls>;
    async fn resolve_http_trigger(&self, public_path: &str) -> Option<(String, TriggerDefinition)>;
    fn owner_run_quota_exceeded(&self, owner: Option<&str>) -> bool;
    async fn start_workflow(
        self: Arc<Self>,
        workflow_id: &str,
        payload: serde_json::Value,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
        parent_instance_id: Option<String>,
    ) -> eyre::Result<String>;
    async fn start_workflow_from_trigger(
        self: Arc<Self>,
        workflow_id: &str,
        trigger: TriggerInfo,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
        run_as: Option<String>,
    ) -> eyre::Result<String>;
    async fn start_workflow_streaming(
        self: Arc<Self>,
        workflow_id: &str,
        trigger: TriggerInfo,
        run_as: Option<String>,
    ) -> eyre::Result<(
        String,
        tokio::sync::broadcast::Receiver<WorkflowStreamEvent>,
    )>;
    async fn resume_workflow(
        self: Arc<Self>,
        instance_id: &str,
        outcome: Option<String>,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
        by: &Steer,
    ) -> eyre::Result<Option<bool>>;
    async fn reject_workflow_step(
        self: Arc<Self>,
        instance_id: &str,
        reason: Option<&str>,
        feedback: Option<&str>,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
        by: &Steer,
    ) -> eyre::Result<Option<bool>>;
    async fn abort_workflow(self: Arc<Self>, instance_id: &str, by: &Steer) -> eyre::Result<()>;
    async fn pause_workflow(&self, instance_id: &str, by: &Steer) -> eyre::Result<()>;
    async fn goto_step(&self, instance_id: &str, step_id: &str, by: &Steer) -> eyre::Result<()>;
    fn retry_step(&self, instance_id: &str, by: &Steer) -> eyre::Result<String>;
    async fn skip_step(self: Arc<Self>, instance_id: &str, by: &Steer) -> eyre::Result<String>;
    async fn authorize_steer(
        &self,
        instance_id: &str,
        steerer: &Steerer,
        action: SteerAction,
    ) -> std::result::Result<Steer, SteerRefused>;
    async fn decide(
        self: Arc<Self>,
        instance_id: &str,
        ask: DecisionAsk,
        steerer: &Steerer,
    ) -> std::result::Result<DecisionMade, DecisionRefused>;
    async fn resume_as(
        self: Arc<Self>,
        id: &str,
        ask: ResumeAsk,
        steerer: &Steerer,
    ) -> std::result::Result<Resumed, ResumeRefused>;
    fn decision_view(
        &self,
        row: &WorkflowInstanceRow,
        viewer: Option<&str>,
    ) -> Option<serde_json::Value>;
    fn decision_reply(
        &self,
        channel: &str,
        thread_ts: &str,
        text: &str,
    ) -> Option<(String, Uuid, String)>;
    fn decision_reminder(&self, row: &WorkflowInstanceRow) -> Option<String>;
    async fn repair_card(
        &self,
        instance_id: &str,
        gate: Uuid,
        channel: &str,
        ts: &str,
        echoed: Option<serde_json::Value>,
        response_url: &str,
    );
    fn get_instance_status(&self, instance_id: &str) -> eyre::Result<Option<WorkflowInstanceRow>>;
    fn list_instances(
        &self,
        status: Option<&str>,
        workflow_id: Option<&str>,
    ) -> eyre::Result<Vec<WorkflowInstanceRow>>;
    async fn blocking_instance(&self, workflow_id: &str) -> eyre::Result<Option<String>>;
    fn find_instance_by_thread(
        &self,
        channel_id: &str,
        thread_id: &str,
        statuses: &[&str],
    ) -> eyre::Result<Option<WorkflowInstanceRow>>;
    async fn readable_runs(
        &self,
        rows: Vec<WorkflowInstanceRow>,
        reader: &DefWriter,
    ) -> Vec<WorkflowInstanceRow>;
    async fn readable_run(
        &self,
        instance_id: &str,
        reader: &DefWriter,
    ) -> eyre::Result<Option<WorkflowInstanceRow>>;
    fn set_operator_variable(
        &self,
        operator: &TurnPrincipal,
        instance_id: &str,
        key: &str,
        value: &str,
    ) -> eyre::Result<()>;
    fn set_instance_variable(&self, instance_id: &str, key: &str, value: &str) -> eyre::Result<()>;
}
