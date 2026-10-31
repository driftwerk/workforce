//! Workflow runtime — executes TOML-defined workflows using the agent service.
//!
//! Mirrors the `Scheduler` pattern: holds `Arc<AgentService>`, loads workflow
//! definitions from disk, and executes steps by calling `process_message()`.

#[cfg(test)]
use crate::serve::Compose as _;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::Utc;
use eyre::{bail, eyre, Result, WrapErr};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use workforce_agent::llm::tiers::TierRegistry;
use workforce_agent::runner::ProgressEvent;
use workforce_common::channels::desk::{DeliveryKind, DeskScope, DeskTarget};
use workforce_common::channels::{
    self, ChannelSurface, OutboundMessage, Reaction, SurfaceRegistry,
};
use workforce_common::persistence::WorkflowInstanceRow;
use workforce_common::session::TurnPrincipal;
use workforce_common::settings::{CodingEngineConfig, CodingEngineKind};
use workforce_common::subscription::WORKFLOW_ENGINE;
use workforce_common::telemetry::TelemetryStore;
use workforce_core::access::{GrantLevel, Visibility};
use workforce_orchestrator::workflow::definition::{
    is_coding_engine, DeliveryMode, TriggerDefinition, VariableDef, WorkflowCall,
    WorkflowDefinition, WorkflowMeta, CHILD_STARTED_OUTCOME, RESERVED_PAYLOAD_KEYS,
};
use workforce_orchestrator::workflow::engine::{
    branch_outcome, parse_report, went_on_with_resumed_outcome, StepExecution, StepMode,
    StepReport, StepTransition, WorkflowExecutor,
};
use workforce_orchestrator::workflow::instance::{
    booked_ticket, step_output, with_step_outputs, CorrectionCause, InstanceStatus, Intervention,
    InterventionKind, OutcomeBy, StepProvenance, TriggerInfo, TriggerSource, WorkflowInstance,
    AGENT_ENGINE, REPORT_ENGINE,
};
use workforce_orchestrator::workflow_store::{
    auto_run_tainted, RunAsWrite, StoredDef, TriggerBinding, KIND_WORKFLOW, TRIGGER_PATH_PREFIX,
};

use crate::code::worktree::WorktreeManager;
use crate::coding_engine::{self, CodingEngine, RoundContext};
use crate::coding_sandbox::{CodingSandbox, HomeSpec, SandboxLauncher, WorkspaceSpec};
use crate::handoff::{Refusal, Verifier};
use crate::hub_core::logins::limits::is_usage_capped;
use crate::remote_mcp::tokens::public_base_url;
use crate::resource_access::DefListing;
use crate::run_steering::{
    RunWorkflow, Steer, SteerAction, SteerRefused, SteerTerms, Steerer, STEERING_KEY,
};
use crate::serve::AgentService;
use crate::team_reach::TeamReach;

mod coding_round;
mod decision;
mod shared_store;

pub use crate::hub_core::workflows::{
    AutomatedRunBlock, TriggerUrls, WorkflowStreamEvent, WorkflowSummary,
};
pub(crate) use decision::awaits_decision;
pub use decision::{DecisionAsk, DecisionMade, DecisionRefused, ResumeAsk, ResumeRefused, Resumed};
pub use shared_store::shared_workflow_store;
use workforce_orchestrator::workflow::decision as run_decision;

/// Substring embedded in every ticket pre-fetch failure error message, so
/// callers and log greps can identify these failures without coupling to
/// per-adapter wording.
const PREFETCH_FAILED_MARKER: &str = "pre-fetch failed";

/// Adapter probe order for ticket pre-fetch: Jira preferred when configured, Linear
/// fallback. Each entry is
/// `(skill_name, tool_name, identifier_param)`.
const TICKET_PREFETCH_ATTEMPTS: &[(&str, &str, &str)] = &[
    ("jira", "jira.get_issue", "issue_key"),
    ("linear", "linear.issue_get", "identifier"),
];

fn format_prefetch_failure(ticket_id: &str, attempted: bool, last_err: Option<&str>) -> String {
    if !attempted {
        return format!(
            "Ticket {PREFETCH_FAILED_MARKER} for {ticket_id}: no Jira or Linear adapter is registered on this agent"
        );
    }
    match last_err {
        Some(err) => format!("Ticket {PREFETCH_FAILED_MARKER} for {ticket_id}: {err}"),
        None => format!("Ticket {PREFETCH_FAILED_MARKER} for {ticket_id}: unknown error"),
    }
}

/// One invocation of the default-engine agent loop within a step.
/// Single-shot steps run exactly one of these; phased steps run one per phase.
struct PhaseCall {
    name: String,
    prompt: String,
    max_rounds: Option<u32>,
    whitelist: Option<Vec<String>>,
}

/// What each phase of a phased step is prompted with. Phases share one
/// session, so a step that was sent back is told why once, as its first phase
/// begins.
fn phase_tasks(exec: &StepExecution) -> impl Iterator<Item = String> + '_ {
    exec.phases.iter().enumerate().map(|(i, phase)| match i {
        0 => format!("{}{}", phase.task, exec.correction),
        _ => phase.task.clone(),
    })
}

/// Expand a step's execution config into the sequence of agent-runtime calls:
/// single-shot → one synthetic entry; phased → one entry per `[[step.phases]]`,
/// each with its own prompt, turn budget, and tool restriction.
fn build_phase_calls(
    exec: &workforce_orchestrator::workflow::engine::StepExecution,
    base_whitelist: Option<Vec<String>>,
) -> Vec<PhaseCall> {
    if exec.phases.is_empty() {
        return vec![PhaseCall {
            name: String::new(),
            prompt: exec.task_prompt.clone(),
            max_rounds: exec.max_rounds,
            whitelist: base_whitelist,
        }];
    }
    phase_tasks(exec)
        .zip(&exec.phases)
        .map(|(prompt, p)| PhaseCall {
            name: p.name.clone(),
            prompt,
            max_rounds: Some(p.max_turns),
            whitelist: Some(p.allowed_tools.clone()),
        })
        .collect()
}

pub struct WorkflowRuntime {
    service: Arc<AgentService>,
    definitions: Arc<RwLock<HashMap<String, OwnedDefinition>>>,
    telemetry_store: Arc<dyn TelemetryStore>,
    workflows_dir: PathBuf,
    surface_registry_override: OnceLock<Arc<tokio::sync::RwLock<SurfaceRegistry>>>,
    /// Optional agent registry for per-step agent routing.
    /// When set, each step's `agent` field is used to resolve the correct AgentService.
    agent_registry: OnceLock<Arc<dyn crate::hosts::RegistryView>>,
    /// Per-agent Slack surfaces: agent_id → surface with that agent's bot token.
    /// Populated from workspace_surfaces + agent_bindings so workflow messages
    /// are sent from the correct bot identity (Martin's bot, Brian's bot, etc.).
    /// Replaced on every Slack reload, so a newly connected agent is included.
    agent_surfaces: parking_lot::RwLock<HashMap<String, Arc<dyn ChannelSurface>>>,
    /// Cancellation tokens for running workflow instances.
    /// abort_workflow() cancels the token to immediately interrupt the running step.
    cancel_tokens: tokio::sync::Mutex<HashMap<String, CancellationToken>>,
    /// Instance ids whose execute loop runs in THIS process — the local half
    /// of the double-run guard (the cross-instance half is the DB lease).
    /// Sync mutex so `save_instance` can consult it.
    active_loops: parking_lot::Mutex<std::collections::HashSet<String>>,
    /// The people whose schedule triggers the last sync left unregistered
    /// because they were suspended; maintenance syncs again once any of them
    /// is not, so a reactivation by any path re-arms them.
    held_schedules: parking_lot::Mutex<std::collections::BTreeSet<String>>,
    /// Serializes every `max_concurrent` decision — a trigger admitting or
    /// queueing a start, and a finished run promoting from the queue — so two
    /// concurrent decisions cannot both see the same free slot. Process-local;
    /// across hub instances the execution lease still prevents a double run.
    capacity: tokio::sync::Mutex<()>,
    /// Process shutdown signal — execute loops break at step boundaries.
    shutdown: std::sync::OnceLock<tokio_util::sync::CancellationToken>,
    /// A verifier used in place of each step agent's own. Tests set one.
    verifier: OnceLock<Arc<dyn Verifier>>,
    /// Decision backends and a recorder used in place of the install's.
    /// Tests set them.
    decisions: OnceLock<Decided>,
    /// Git worktree manager for per-instance repo isolation.
    worktree_manager: OnceLock<Arc<WorktreeManager>>,
    /// Where a `claude-code` step runs when the install's coding executor is
    /// remote: a scope on the coding worker replaces the local worktree.
    remote_launcher: OnceLock<Arc<dyn SandboxLauncher>>,
    /// What runs a `claude-code` step and what it falls back to; Claude Code on
    /// the subscription alone until the install configures otherwise.
    coding_engines: OnceLock<(Arc<coding_engine::Engines>, CodingEngineConfig)>,
    /// The tool patterns a Claude Code step is served over MCP; unset gives
    /// steps no hub tools.
    mcp_exposed_tools: OnceLock<Vec<String>>,
    /// DB-backed definition store (DW-550). When set, definitions are read
    /// from / written to Postgres instead of `workflows_dir`; the directory
    /// remains the fallback for non-Postgres deployments.
    store: OnceLock<Arc<workforce_orchestrator::workflow_store::WorkflowStore>>,
    /// Where every public trigger path leads, and which paths each definition
    /// answers on, rebuilt on every load. Replaced in a single write so a
    /// reader sees the old index or the new one, never a partial one.
    trigger_index: Arc<RwLock<TriggerIndex>>,
    /// Per-instance broadcast senders for streamed (SSE) runs. Present only
    /// while a streaming caller is attached; `execute_loop` emits step events
    /// into the matching sender when one exists.
    live_streams: Arc<RwLock<HashMap<String, tokio::sync::broadcast::Sender<WorkflowStreamEvent>>>>,
}

/// Removes an instance's streaming sender when the executing task ends — on
/// success, error, or panic — so `live_streams` cannot leak.
struct StreamGuard {
    runtime: Arc<WorkflowRuntime>,
    instance_id: String,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let runtime = Arc::clone(&self.runtime);
        let instance_id = std::mem::take(&mut self.instance_id);
        tokio::spawn(async move {
            runtime.live_streams.write().await.remove(&instance_id);
        });
    }
}

/// A loaded definition plus its ownership metadata. Member-owned definitions
/// are addressed by `{owner}/{id}` (see [`def_key`]); system/disk definitions by
/// bare `id`. `owner == None` marks an operator/disk definition.
#[derive(Clone)]
pub struct OwnedDefinition {
    pub def: WorkflowDefinition,
    pub owner: Option<String>,
    pub visibility: Visibility,
    /// Identity that last wrote this definition; gates automated runs against a
    /// share grantee's edit (see [`workforce_orchestrator::workflow_store::auto_run_tainted`]).
    pub last_editor: Option<String>,
    /// Identity automated runs execute as instead of the owner (a manage-share
    /// grantee who saved with `run_as = me`).
    pub run_as: Option<String>,
    /// BLAKE3 of the stored TOML this definition was parsed from, stamped on
    /// every step record it runs; `None` for disk-seeded definitions.
    pub content_hash: Option<String>,
}

impl OwnedDefinition {
    fn key(&self) -> String {
        def_key(self.owner.as_deref(), &self.def.workflow.id)
    }
}

/// Wrap a disk/operator definition as system-owned + globally runnable.
fn system_def(def: WorkflowDefinition) -> OwnedDefinition {
    OwnedDefinition {
        def,
        owner: None,
        visibility: Visibility::Global,
        last_editor: None,
        run_as: None,
        content_hash: None,
    }
}

/// Operator-curated namespace a member-owned definition may reference for
/// shared (`agent`/`global`) secrets. Only operators can write shared vault
/// tiers, so a member can reference a shared secret only after one is placed
/// here — infra credentials outside it stay unreferenceable.
const WORKFLOW_SHARED_SECRET_PREFIX: &str = "workflow_shared/";

/// Whether `owner` owns at least `cap` of the given running addressing keys.
/// Matches on the `{owner}/` prefix, so `alice` never counts `alice2`'s runs
/// and never counts a system definition's bare-id instance.
fn quota_exceeded(running_ids: &[String], owner: &str, cap: usize) -> bool {
    let prefix = format!("{owner}/");
    running_ids
        .iter()
        .filter(|id| id.starts_with(&prefix))
        .count()
        >= cap
}

/// The owner of a composite addressing key, or `None` for a bare (system) id.
fn owner_of_key(key: &str) -> Option<&str> {
    key.split_once('/').map(|(owner, _)| owner)
}

/// Composite addressing key. System/disk definitions keep their bare id so
/// existing run endpoints and external webhooks are unaffected; member-owned
/// definitions are namespaced by owner so two members may share an id.
pub(crate) fn def_key(owner: Option<&str>, id: &str) -> String {
    match owner {
        Some(o) => format!("{o}/{id}"),
        None => id.to_string(),
    }
}

/// The identity a run executes as. Interactive runs use the runner; automated
/// runs (webhook/schedule/non-human) of a member-owned definition use its run
/// identity — the stored `run_as`, else the owner. System/disk definitions
/// (no owner) fall back to any explicit `run_as`, which only operator-trusted
/// definitions may declare.
fn derive_execution_identity(
    owner: Option<&str>,
    run_as: Option<&str>,
    automated: bool,
    caller: Option<String>,
) -> Option<String> {
    match (automated, owner) {
        (true, Some(owner)) => Some(run_as.unwrap_or(owner).to_string()),
        _ => caller,
    }
}

/// The first reason an automated run as `run_identity` must not fire: foreign
/// content (taint), a run identity the owner has since un-shared, or an
/// inactive identity. `shared` is whether the run identity may still manage
/// the definition; `active` whether it is an active identity.
fn automated_run_block_reason(
    owner: Option<&str>,
    last_editor: Option<&str>,
    run_identity: Option<&str>,
    shared: bool,
    active: bool,
) -> Option<AutomatedRunBlock> {
    if auto_run_tainted(owner, last_editor, run_identity) {
        Some(AutomatedRunBlock::Tainted)
    } else if !shared {
        Some(AutomatedRunBlock::Unshared)
    } else if !active {
        Some(AutomatedRunBlock::Inactive)
    } else {
        None
    }
}

/// What a schedule sync does with one trigger.
enum Registration {
    Armed(Box<ScheduleRegistration>),
    /// Left unregistered while the person it runs as is suspended.
    Held(String),
    Skipped,
}

/// The owner-derived ("legacy") public trigger path: bare `id` for a system
/// definition, `{owner}/{id}` for a member one. Built here and nowhere else.
///
/// A member one names its owner, so it cannot survive the workflow changing
/// hands — which is why every trigger also gets a minted path that ownership
/// is not an input to
/// ([`workforce_orchestrator::workflow_store::mint_trigger_path`]). These keep
/// answering indefinitely, for the callers already configured with them.
fn trigger_public_id(owner: Option<&str>, trigger_id: &str) -> String {
    match owner {
        Some(o) => format!("{o}/{trigger_id}"),
        None => trigger_id.to_string(),
    }
}

/// Compute a run's secret policy from its ownership, executing identity, and
/// declared variables. Per-user secrets resolve only when the executor owns the
/// definition (a system/disk definition lets its `run_as` identity read its
/// own); member definitions confine shared-scope references to the curated
/// `prefix`; the declared set is the secret variables' `(scope, key)` pairs and
/// their egress allowlists.
fn compute_secret_policy(
    owner: Option<&str>,
    executing: Option<String>,
    variables: &[VariableDef],
    prefix: &str,
) -> workforce_common::secret_policy::SecretPolicy {
    use workforce_common::secret_policy::{DeclaredSecret, SecretPolicy};
    use workforce_common::vault_scope::parse_exact_scoped_marker;

    let per_user_identity = match owner {
        Some(owner) => (executing.as_deref() == Some(owner)).then(|| owner.to_string()),
        None => executing,
    };
    let shared_secret_prefix = owner.map(|_| prefix.to_string());
    let declared = variables
        .iter()
        .filter(|v| v.is_secret())
        .filter_map(|v| {
            let m = parse_exact_scoped_marker(&v.value)?;
            Some((
                (m.scope_name, m.key),
                DeclaredSecret {
                    allowed_hosts: v.allowed_hosts.clone(),
                },
            ))
        })
        .collect();
    SecretPolicy {
        per_user_identity,
        shared_secret_prefix,
        declared,
    }
}

/// The `CURRENT_USER_ID` currently in scope, if any. Captured before a detached
/// `tokio::spawn` so the spawned run can re-apply it — a bare spawn drops
/// task-locals, which would silently unscope per-user resolution.
fn current_scoped_user() -> Option<String> {
    workforce_agent::runner::CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty())
}

/// Persist the executing identity (`_run_as`) and whom the run is billed to
/// (`_bill_to`) on the instance, so every later entry into the loop — resume,
/// startup recovery, the maintenance sweep — runs the steps as the same
/// principal, not as the synthetic engine user. The capability gate and
/// per-user vault/memory resolution key on the one, spend on the other.
fn persist_principal(instance: &mut WorkflowInstance, principal: &TurnPrincipal) {
    if let Some(obj) = instance.variables.as_object_mut() {
        for (key, id) in [(RUN_AS, &principal.run_as), (BILL_TO, &principal.bill_to)] {
            if let Some(id) = id {
                obj.insert(key.to_string(), serde_json::json!(id));
            }
        }
    }
}

const RUN_AS: &str = "_run_as";
const BILL_TO: &str = "_bill_to";

/// The steering terms `owned` sets today.
fn steer_terms(owned: &OwnedDefinition) -> SteerTerms {
    SteerTerms {
        by_others: owned.def.workflow.steerable_by_others,
        visibility: owned.visibility,
    }
}

/// A run's workflow as the steering rule reads it: the terms that bind a run
/// whose `variables` kept those it started under.
fn run_workflow(owned: &OwnedDefinition, variables: &serde_json::Value) -> RunWorkflow {
    RunWorkflow {
        id: owned.def.workflow.id.clone(),
        terms: SteerTerms::binding(SteerTerms::kept_in(variables), steer_terms(owned)),
    }
}

/// Keep the steering terms the run starts under, which bind it unless its
/// workflow's become stricter ([`SteerTerms::binding`]).
fn persist_steering(instance: &mut WorkflowInstance, owned: &OwnedDefinition) {
    if let Some(obj) = instance.variables.as_object_mut() {
        obj.insert(STEERING_KEY.to_string(), steer_terms(owned).to_value());
    }
}

/// The principal a run starts under from a trigger: `identity`, billed as the
/// work in scope is.
fn launch_principal(identity: Option<String>) -> TurnPrincipal {
    TurnPrincipal {
        run_as: identity,
        bill_to: crate::user_config::explicit_billing_identity(),
    }
}

/// Seed the engine-owned delivery keys from the start call's own trusted
/// arguments. `WorkflowInstance::new` strips `_…` keys out of trigger
/// payloads (an external body must not smuggle identity or routing), so the
/// engine is the only writer: `save_instance` derives the thread-index
/// columns from these variables, and Slack command matching
/// (approve/reject/stop in the run's thread) depends on them.
/// A run started from a Desk conversation also remembers it (`_desk`), so
/// what the run says outside any turn reaches that conversation.
fn persist_delivery_context(
    instance: &mut WorkflowInstance,
    channel_id: Option<&str>,
    thread_id: Option<&str>,
    desk: Option<&DeskTarget>,
) {
    if let Some(obj) = instance.variables.as_object_mut() {
        if let Some(ch) = channel_id {
            obj.insert("_channel_id".to_string(), serde_json::json!(ch));
        }
        if let Some(th) = thread_id {
            obj.insert("_thread_id".to_string(), serde_json::json!(th));
        }
        if let Some(desk) = desk.and_then(|desk| serde_json::to_value(desk).ok()) {
            obj.insert(DESK_KEY.to_string(), desk);
        }
    }
}

/// The instance variable holding the Desk conversation a run reports to.
const DESK_KEY: &str = "_desk";

/// The Desk conversation `instance` reports to, if it was started from one.
pub(crate) fn desk_of(instance: &WorkflowInstance) -> Option<DeskTarget> {
    instance
        .variables
        .get(DESK_KEY)
        .and_then(|desk| serde_json::from_value(desk.clone()).ok())
}

/// Where a run's messages go: the Slack channel (and thread) it was started
/// in, else the Desk conversation it answers to, else nowhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Destination<'a> {
    Slack {
        channel: &'a str,
        thread: Option<&'a str>,
    },
    Desk(DeskScope),
    Dark,
}

pub(crate) fn destination_of<'a>(
    channel_id: Option<&'a str>,
    thread_id: Option<&'a str>,
    desk: Option<DeskScope>,
) -> Destination<'a> {
    match (channel_id, desk) {
        (Some(channel), _) => Destination::Slack {
            channel,
            thread: thread_id,
        },
        (None, Some(desk)) => Destination::Desk(desk),
        (None, None) => Destination::Dark,
    }
}

/// The Slack channel and thread a saved instance delivers to, as
/// [`persist_delivery_context`] recorded them.
fn delivery_context(instance: &WorkflowInstance) -> (Option<String>, Option<String>) {
    let var = |key: &str| {
        instance
            .variables
            .get(key)
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    (var("_channel_id"), var("_thread_id"))
}

/// Render a skill tool's JSON output as a step response the existing
/// machinery parses: the payload, an OUTPUTS block for declared fields
/// present at the top level, and an OUTCOME line when the result carries an
/// `outcome` string (routing branches exactly like an agent's OUTCOME line).
/// Schedule jobs that no longer back a live registration, keyed on **id**. A
/// store that already holds same-named duplicates — every pre-fix reload added
/// one — keeps them if only unknown names are swept, because the name is still
/// desired.
pub(crate) fn stale_schedule_job_ids<'a>(
    existing: impl IntoIterator<Item = (&'a str, &'a str)>,
    live: &std::collections::HashSet<String>,
) -> Vec<String> {
    existing
        .into_iter()
        .filter(|(id, name)| name.starts_with(SCHEDULE_JOB_PREFIX) && !live.contains(*id))
        .map(|(id, _)| id.to_string())
        .collect()
}

/// Artifact scope shared by every step of one workflow instance.
fn artifact_scope(instance_id: &str) -> String {
    format!("workflow:{instance_id}")
}

/// Name prefix for the scheduler jobs that back `TriggerDefinition::Schedule`.
const SCHEDULE_JOB_PREFIX: &str = "workflow-schedule:";

/// One scheduler job derived from a schedule trigger.
struct ScheduleRegistration {
    name: String,
    description: String,
    schedule: crate::hub_core::schedules::Schedule,
    payload: crate::hub_core::schedules::JobPayload,
    identity: Option<String>,
}

/// Where a scheduled run delivers progress and terminal status. Authors already
/// express it as `channel_id` in the trigger payload; without threading it onto
/// the job a scheduled run has no surface and fails silently.
fn trigger_delivery_channel(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("channel_id")
        .and_then(serde_json::Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}

/// Why a run of `def` must not start on this hub: each skill its `requires`
/// names that is not installed, or installed at a version the requirement
/// does not accept. `None` when every requirement holds.
fn unmet_requirements(def: &WorkflowDefinition) -> Option<String> {
    if def.workflow.requires.is_empty() {
        return None;
    }
    let installed: HashMap<String, String> = crate::wasm_skill_tools::global_skill_store()
        .map(|store| {
            store
                .list_installed()
                .into_iter()
                .map(|skill| (skill.manifest.name, skill.manifest.version))
                .collect()
        })
        .unwrap_or_default();
    let unmet = def
        .workflow
        .unmet_requirements(|skill| installed.get(skill).map(String::as_str));
    (!unmet.is_empty()).then(|| unmet.join("; "))
}

/// Tool steps naming a tool its installed skill holds behind a human
/// decision. Such a step can never run — a workflow has nobody to ask — but a
/// skill can gain a gate long after a workflow was written, so this reports a
/// notice rather than rejecting the save.
fn approval_gated_tool_steps(
    def: &WorkflowDefinition,
    installed: &[workforce_wasm_runtime::InstalledWasmSkill],
) -> Vec<String> {
    let gated: BTreeSet<String> = installed
        .iter()
        .flat_map(|skill| {
            skill
                .manifest
                .tools
                .iter()
                .filter(|tool| tool.requires_approval)
                .map(|tool| format!("{}.{}", skill.manifest.name, tool.name))
        })
        .collect();
    def.steps
        .iter()
        .filter_map(|step| step.tool.as_deref())
        .filter(|tool| gated.contains(*tool))
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// A tool step's input: its resolved task as a JSON object, `{}` when blank,
/// and plain text wrapped as `{"input": …}`. A task that looks like JSON but
/// does not parse is an authoring error, not an input.
fn tool_step_input(resolved_task: &str) -> Result<serde_json::Value, String> {
    match resolved_task.trim() {
        "" => Ok(serde_json::json!({})),
        text if text.starts_with('{') => serde_json::from_str::<serde_json::Value>(text)
            .map_err(|e| format!("task resolved to invalid JSON: {e}"))
            .and_then(|value| {
                value
                    .is_object()
                    .then_some(value)
                    .ok_or_else(|| "task resolved to JSON that is not an object".to_string())
            }),
        text => Ok(serde_json::json!({ "input": text })),
    }
}

/// Run a tool step's single tool call. No agent turn runs here, so the step
/// establishes the agent id the WASM host offloads an over-limit result under.
async fn dispatch_tool_step(
    tools: &workforce_agent::tools::registry::ToolRegistry,
    agent_id: &str,
    tool_name: &str,
    input: serde_json::Value,
    declared_outputs: &[String],
    validation: workforce_agent::tools::arguments::ArgumentValidation,
) -> Result<String, String> {
    if let Some(refusal) = workforce_agent::tools::dispatch::unattended_refusal(tools, tool_name) {
        return Err(refusal);
    }
    let Some(tool) = tools.get(tool_name) else {
        return Err(format!("tool '{tool_name}' is not registered"));
    };
    if let Some(workforce_agent::tools::dispatch::GateOutcome::Blocked { reason, .. }) =
        workforce_agent::tools::dispatch::argument_gate(
            validation, agent_id, tools, tool_name, &input,
        )
    {
        return Err(format!("'{tool_name}' did not run: {reason}"));
    }
    let result = workforce_agent::runner::CURRENT_AGENT_ID
        .scope(agent_id.to_string(), tool.execute(input))
        .await
        .map_err(|e| e.to_string())?;
    if !result.success {
        return Err(result
            .error
            .unwrap_or_else(|| "skill tool returned failure".to_string()));
    }
    Ok(render_tool_step_response(&result.output, declared_outputs))
}

pub(crate) fn render_tool_step_response(output: &serde_json::Value, declared: &[String]) -> String {
    let mut text = serde_json::to_string_pretty(output).unwrap_or_else(|_| output.to_string());
    if let Some(obj) = output.as_object() {
        let fields: Vec<String> = declared
            .iter()
            .filter_map(|name| {
                obj.get(name).map(|v| match v {
                    serde_json::Value::String(s) => format!("{name}: {s}"),
                    other => format!("{name}: {other}"),
                })
            })
            .collect();
        if !fields.is_empty() {
            text.push_str(&format!("\n\nOUTPUTS:\n{}\nEND_OUTPUTS", fields.join("\n")));
        }
        if let Some(outcome) = obj.get("outcome").and_then(|v| v.as_str()) {
            text.push_str(&format!("\nOUTCOME: {outcome}"));
        }
    }
    text
}

/// The package a step's response hands on, once it reads cleanly and — where
/// the step or its agent asks for one — a verifier has passed it. A response
/// whose `OUTCOME:` selects a declared branch is leaving the fall-through
/// path its outputs describe, so it owes no missing field and is not judged
/// against them. Only a model turn
/// is sent back: a tool, decision or child run may already have acted, and
/// running it again repeats the side effect without changing its answer.
async fn assess_handover(
    step: &workforce_orchestrator::workflow::definition::StepDefinition,
    exec: &StepExecution,
    service: &Arc<AgentService>,
    instance: &WorkflowInstance,
    response: &str,
    verifier: Option<&Arc<dyn Verifier>>,
    step_cost_usd: &mut f64,
) -> std::result::Result<Option<serde_json::Value>, Refusal> {
    let sent_back = |cause| instance.corrections_of(&step.id, cause);
    let branched = branch_outcome(step, response).is_some();
    let package = crate::handoff::package_of(
        response,
        &step.outputs,
        branched,
        !exec.mode.is_delegated()
            && crate::handoff::repairs_left(sent_back(CorrectionCause::Malformed)),
    )?;
    let policy = service.handoff();
    if exec.mode != StepMode::Agent
        || branched
        || !crate::handoff::wants_verifier(&policy, &step.outputs, step.verify.as_ref())
    {
        return Ok(package);
    }
    let verifier: Arc<dyn Verifier> = match verifier {
        Some(verifier) => Arc::clone(verifier),
        None => Arc::new(
            crate::handoff::handoff_verifier(
                service,
                &policy,
                workforce_common::subscription::Origin::Workflow,
                crate::user_config::current_billing_identity_id(),
            )
            .await,
        ),
    };
    let rounds = policy.corrections(step.verify.as_ref().and_then(|verify| verify.corrections));
    let verdict = crate::handoff::verified(
        verifier.as_ref(),
        &crate::handoff::Brief::new(&exec.task, &step.outputs, step.verify.as_ref()),
        response,
        package.as_ref(),
        sent_back(CorrectionCause::Rejected) < rounds,
    )
    .await;
    *step_cost_usd += verifier.spent();
    verdict.map(|()| package)
}

/// Decision backends and the recorder their records are handed to.
type Decided = (
    Arc<crate::decide::Decisions>,
    Option<crate::decision_records::Recorder>,
);

/// Execution-lease timing: the loop renews well inside the TTL, so an
/// expired lease reliably means the owning process is gone (or wedged past
/// any reasonable step-bookkeeping stall — steps themselves can run long,
/// the renewal task ticks independently).
/// How many times a change made outside a run's loop is read and applied
/// again before it gives up on a run that keeps changing.
const FENCE_ATTEMPTS: usize = 5;
/// How often waiting decisions are checked for an expiry or unposted cards.
const DECISION_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const WORKFLOW_LEASE_TTL_SECS: i64 = 120;
const WORKFLOW_LEASE_RENEW_SECS: u64 = 30;
/// Marker in fenced-out save errors so the loop wrapper can swallow them —
/// a fenced-out loop must stop quietly, not post a terminal failure the new
/// owner's run would contradict.
const FENCED: &str = "[lease-fenced]";
/// Marker in the error for a run whose step history this build cannot
/// read: its loop stops as quietly, leaving the run to a build that can.
const UNREADABLE: &str = "[history-unreadable]";

/// Stable id for this hub process — the workflow execution lease owner.
pub(crate) fn process_instance_id() -> &'static str {
    workforce_common::instance::process_instance_id()
}

/// Whether the process is shutting down — set once by the serve wiring;
/// execute loops break at the next step boundary so a successor's recovery
/// resumes the instance instead of a hard kill mid-step.
fn shutdown_cancelled(token: &std::sync::OnceLock<tokio_util::sync::CancellationToken>) -> bool {
    token.get().is_some_and(|t| t.is_cancelled())
}

/// How long a step whose command runner has just closed waits for the
/// process's own drain signal before concluding the runner was lost on its
/// own: init stops the hub and its runner together, so a restart shows up
/// within milliseconds, while a worker outage never does.
const RESTART_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Whether a failed step's cause is the runner having closed under it.
fn runner_lost(error: &str) -> bool {
    error.contains("secure command runner unavailable")
}

/// The persisted principal; empty on an instance that carries none.
fn instance_principal(instance: &WorkflowInstance) -> TurnPrincipal {
    principal_of(&instance.variables)
}

fn principal_of(variables: &serde_json::Value) -> TurnPrincipal {
    let id = |key| {
        variables
            .get(key)
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    TurnPrincipal {
        run_as: id(RUN_AS),
        bill_to: id(BILL_TO),
    }
}

/// Whether a run's workflow reaches one reader: the share and team lookups
/// [`crate::def_write_auth::may_read_def`] makes for them.
struct Reach<'a> {
    reader: &'a crate::def_write_auth::DefWriter,
    teams: TeamReach,
    shares: crate::def_write_auth::StoredShares,
}

impl Reach<'_> {
    /// Whether `workflow`, owned by `owner`, reaches the reader under the
    /// visibility that binds the run.
    async fn reaches(&self, workflow: &RunWorkflow, owner: Option<&str>) -> bool {
        crate::def_write_auth::may_read_def(
            self.reader,
            &self.shares,
            KIND_WORKFLOW,
            &workflow.id,
            owner,
            workflow.terms.visibility,
            &self.teams,
        )
        .await
    }
}

/// Whether a run is `reader`'s to see without looking at its workflow. Its
/// step outputs are what it read on someone's behalf, so the people it runs as
/// or is billed to, and an administrator, reach it. A run that names no person
/// (none at all, or only a `system:` or `agent:` lane) is a system run and
/// stays visible to every reader; one whose variables do not parse cannot say
/// whose it is, so only an administrator sees it. The whole rule, which also
/// opens a run to whoever may steer it, is
/// [`WorkflowRuntime::readable_runs`].
pub(crate) fn run_is_for(
    row: &WorkflowInstanceRow,
    reader: &crate::def_write_auth::DefWriter,
) -> bool {
    let Ok(variables) = serde_json::from_str::<serde_json::Value>(&row.variables) else {
        return reader.is_admin;
    };
    reader.is_admin
        || principal_of(&variables).readable_by(&TurnPrincipal::run_as(reader.identity_id.clone()))
}

/// Each HTTP trigger id a definition declares, once. `validate()` checks the
/// charset of every HTTP trigger id but does not reject a definition that
/// declares the same one twice, so the reader collapses it.
fn http_trigger_ids(owned: &OwnedDefinition) -> impl Iterator<Item = &String> {
    let mut seen = HashSet::new();
    owned
        .def
        .triggers
        .iter()
        .filter_map(|trigger| match trigger {
            TriggerDefinition::Http { id, .. } => Some(id),
            _ => None,
        })
        .filter(move |id| seen.insert(id.as_str()))
}

/// The store binding of one HTTP trigger: the value a minted public path is
/// keyed by, and the one an ownership transfer moves.
fn trigger_binding(owned: &OwnedDefinition, trigger_id: &str) -> TriggerBinding {
    TriggerBinding {
        kind: KIND_WORKFLOW.to_string(),
        def_id: owned.def.workflow.id.clone(),
        owner: owned.owner.clone(),
        trigger_id: trigger_id.to_string(),
    }
}

/// Every binding the loaded definitions declare — the set the store reconciles
/// its `trigger_endpoints` rows against.
fn trigger_bindings(defs: &HashMap<String, OwnedDefinition>) -> HashSet<TriggerBinding> {
    defs.values()
        .flat_map(|owned| http_trigger_ids(owned).map(|id| trigger_binding(owned, id)))
        .collect()
}

/// A stored row as a loaded definition, keyed for the definitions map, or
/// `None` with a warning when it no longer parses: one unparseable row must
/// not fail the whole reload.
fn stored_to_owned(row: StoredDef) -> Option<(String, OwnedDefinition)> {
    match WorkflowDefinition::from_toml(&row.toml_source) {
        Ok(def) => {
            let owned = OwnedDefinition {
                def,
                owner: row.owner,
                visibility: row.visibility,
                last_editor: row.last_editor,
                run_as: row.run_as,
                content_hash: Some(workforce_orchestrator::workflow_store::content_hash(
                    &row.toml_source,
                )),
            };
            Some((owned.key(), owned))
        }
        Err(e) => {
            warn!(id = %row.id, error = %e, "Skipping invalid workflow def in DB");
            None
        }
    }
}

/// One HTTP trigger's two candidate addresses, before the conflict sweep.
struct TriggerEntry {
    def_key: String,
    trigger_id: String,
    legacy: String,
    stable: Option<String>,
}

/// Legacy paths claimed by more than one definition. Ambiguity fails closed:
/// the path is dropped from the index (404) and logged, rather than routing to
/// an arbitrary workflow.
fn conflicting_legacy_paths<'a>(entries: &'a [TriggerEntry]) -> HashSet<&'a str> {
    let mut claimed: HashMap<&'a str, &'a str> = HashMap::new();
    let mut conflicts = HashSet::new();
    for entry in entries {
        match claimed.insert(&entry.legacy, &entry.def_key) {
            Some(existing) if existing != entry.def_key => {
                error!(
                    http_trigger_path = %entry.legacy,
                    first = %existing,
                    second = %entry.def_key,
                    "Duplicate HTTP trigger path across definitions — endpoint disabled"
                );
                conflicts.insert(entry.legacy.as_str());
            }
            _ => {}
        }
    }
    conflicts
}

/// An entry's legacy path while it still answers. A path dropped in the
/// conflict sweep must read as absent everywhere, or a surface would offer a
/// copyable URL that 404s.
fn live_legacy<'a>(entry: &'a TriggerEntry, conflicts: &HashSet<&str>) -> Option<&'a str> {
    (!conflicts.contains(entry.legacy.as_str())).then_some(entry.legacy.as_str())
}

/// Where an inbound public path leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TriggerRoute {
    /// The definition's addressing key (`def_key`).
    pub def_key: String,
    /// The trigger's own id inside that definition.
    pub trigger_id: String,
}

/// The public paths of one trigger. `stable` is absent only on a hub with no
/// definition store, where nothing is minted; `legacy` is absent where the
/// owner-derived path was dropped for a collision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TriggerPaths {
    pub trigger_id: String,
    pub stable: Option<String>,
    pub legacy: Option<String>,
}

/// Both directions, derived in one pass: what an inbound path resolves to, and
/// what paths a definition should be shown.
#[derive(Debug, Default)]
pub(crate) struct TriggerIndex {
    by_path: HashMap<String, TriggerRoute>,
    by_def: HashMap<String, Vec<TriggerPaths>>,
}

impl TriggerIndex {
    /// Pure: no I/O, no clock, no randomness. `endpoints` is the minted
    /// stable-path overlay; an empty overlay yields a legacy-only index, which
    /// is what keeps every address that resolves today resolving when the
    /// endpoint store is absent or erroring.
    ///
    /// A minted path can never conflict — with another minted one
    /// (`public_path` is a primary key) or with a legacy one (a legacy path
    /// either contains `/` or cannot contain `.`) — so the conflict sweep is
    /// reachable only for legacy paths, and `by_def` is filled from what
    /// survives it.
    pub(crate) fn build(
        defs: &HashMap<String, OwnedDefinition>,
        endpoints: &HashMap<TriggerBinding, String>,
    ) -> Self {
        let entries: Vec<TriggerEntry> = defs
            .iter()
            .flat_map(|(def_key, owned)| {
                http_trigger_ids(owned).map(move |id| TriggerEntry {
                    def_key: def_key.clone(),
                    trigger_id: id.clone(),
                    legacy: trigger_public_id(owned.owner.as_deref(), id),
                    stable: endpoints.get(&trigger_binding(owned, id)).cloned(),
                })
            })
            .collect();
        let conflicts = conflicting_legacy_paths(&entries);

        let by_path = entries
            .iter()
            .flat_map(|entry| {
                let route = TriggerRoute {
                    def_key: entry.def_key.clone(),
                    trigger_id: entry.trigger_id.clone(),
                };
                entry
                    .stable
                    .clone()
                    .into_iter()
                    .chain(live_legacy(entry, &conflicts).map(str::to_string))
                    .map(move |path| (path, route.clone()))
            })
            .collect();
        let by_def = entries.iter().fold(
            HashMap::<String, Vec<TriggerPaths>>::new(),
            |mut acc, entry| {
                acc.entry(entry.def_key.clone())
                    .or_default()
                    .push(TriggerPaths {
                        trigger_id: entry.trigger_id.clone(),
                        stable: entry.stable.clone(),
                        legacy: live_legacy(entry, &conflicts).map(str::to_string),
                    });
                acc
            },
        );
        Self { by_path, by_def }
    }

    pub(crate) fn route(&self, path: &str) -> Option<TriggerRoute> {
        self.by_path.get(path).cloned()
    }

    pub(crate) fn paths_for(&self, def_key: &str) -> &[TriggerPaths] {
        self.by_def.get(def_key).map_or(&[], Vec::as_slice)
    }
}

/// Which addressing form a caller used. The one new observability this change
/// adds, and the evidence any later retirement of the legacy forms would be
/// decided on.
pub(crate) fn trigger_path_form(public_path: &str) -> &'static str {
    match public_path.starts_with(TRIGGER_PATH_PREFIX) {
        true => "stable",
        false => "legacy",
    }
}

/// One trigger's paths as absolute URLs. The single renderer: no surface
/// formats a trigger URL itself.
fn trigger_urls_of(base: &str, paths: &TriggerPaths) -> TriggerUrls {
    let absolute = |path: &str| format!("{base}/triggers/{path}");
    TriggerUrls {
        trigger_id: paths.trigger_id.clone(),
        url: paths.stable.as_deref().map(absolute),
        legacy_url: paths.legacy.as_deref().map(absolute),
    }
}

/// The active instance that fills the last of `cap` slots, if the slots are
/// all taken.
fn blocking(active: &[String], cap: u32) -> Option<&String> {
    (active.len() >= cap.max(1) as usize)
        .then(|| active.last())
        .flatten()
}

/// Variable that holds a tool step's answer from the moment the tool
/// returns until the step is recorded: the window an interrupted loop
/// (restart, panic, orphan recovery) would otherwise re-enter by running the
/// tool again. A skill tool has side effects — it claims a ticket, posts a
/// comment, writes a record — so the answer is kept, not the call.
const TOOL_REPORTED_KEY: &str = "_tool_reported";

/// Keep `response` as the answer `step_id`'s tool already gave, so a loop
/// resumed at this step reuses it instead of calling the tool again.
fn note_reported_tool_result(instance: &mut WorkflowInstance, step_id: &str, response: &str) {
    if let Some(vars) = instance.variables.as_object_mut() {
        vars.insert(
            TOOL_REPORTED_KEY.to_string(),
            serde_json::json!({ "step": step_id, "response": response }),
        );
    }
}

/// The answer `step_id`'s tool gave before the loop was interrupted, taken
/// once: the note is cleared so the answer is not reused a second time.
/// `None` when there is no note or it belongs to another step.
fn take_reported_tool_result(instance: &mut WorkflowInstance, step_id: &str) -> Option<String> {
    let vars = instance.variables.as_object_mut()?;
    let noted_step = vars
        .get(TOOL_REPORTED_KEY)
        .and_then(|v| v.get("step"))
        .and_then(|v| v.as_str())
        .map(str::to_string)?;
    if noted_step != step_id {
        return None;
    }
    vars.remove(TOOL_REPORTED_KEY).and_then(|v| {
        v.get("response")
            .and_then(|r| r.as_str())
            .map(str::to_string)
    })
}

/// Variable that marks a workflow step as having started (or being about to
/// start) its child run, so a loop resumed at this step attaches to that run
/// instead of starting another. Cleared once the step has its answer.
const CHILD_STARTED_KEY: &str = "_child_started";
/// How deep workflow-calls-workflow may nest.
const MAX_WORKFLOW_CALL_DEPTH: usize = 8;
/// How often a waiting workflow step looks at its child run.
const CHILD_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// What a delegated step's answer is shaped by: the labels it can route on
/// and the outputs it declares.
struct StepShape {
    branches: Vec<String>,
    declared: Vec<String>,
}

/// What a delegated step runs with besides its definition: the service a
/// tool runs in, where the run posts, and the agent it runs as.
struct DelegatedContext<'a> {
    step_service: &'a Arc<AgentService>,
    channel_id: Option<&'a str>,
    thread_id: Option<&'a str>,
    step_agent_id: &'a str,
}

/// Where a child run started by a step posts, and the agent that started it.
struct ChildLaunch<'a> {
    agent_id: &'a str,
    channel_id: Option<&'a str>,
    thread_id: Option<&'a str>,
}

/// How a fresh instance is launched: where it posts, who asked, whose
/// identity it runs under, and the run it is a child of.
struct RunLaunch {
    channel_id: Option<String>,
    thread_id: Option<String>,
    originating_agent_id: Option<String>,
    identity: Option<String>,
    parent_instance_id: Option<String>,
}

fn note_child_started(instance: &mut WorkflowInstance, step_id: &str) {
    if let Some(vars) = instance.variables.as_object_mut() {
        vars.insert(
            CHILD_STARTED_KEY.to_string(),
            serde_json::json!({ "step": step_id, "since": chrono::Utc::now().to_rfc3339() }),
        );
    }
}

fn note_child_instance(instance: &mut WorkflowInstance, child_id: &str) {
    if let Some(note) = instance.variables.get_mut(CHILD_STARTED_KEY) {
        note["instance_id"] = serde_json::json!(child_id);
    }
}

fn clear_child_started(instance: &mut WorkflowInstance) {
    if let Some(vars) = instance.variables.as_object_mut() {
        vars.remove(CHILD_STARTED_KEY);
    }
}

fn run_ended(status: &str) -> bool {
    status
        .parse::<InstanceStatus>()
        .is_ok_and(|s| s.is_terminal())
}

/// Why a child's end halts its caller: it failed or was cancelled and no
/// branch claims that status. `None` when the caller routes or falls through.
fn unhandled_child_end(
    row: &WorkflowInstanceRow,
    output: &serde_json::Value,
    branches: &[String],
) -> Option<String> {
    let status = row.status.as_str();
    if !matches!(status, "failed" | "cancelled") || branches.iter().any(|b| b == status) {
        return None;
    }
    let reason = output
        .get("failure_reason")
        .or_else(|| output.get("cancellation_reason"))
        .and_then(|v| v.as_str())
        .unwrap_or("no reason recorded");
    Some(format!("(instance {}) {status}: {reason}", row.id))
}

/// A workflow step's resolved task as the child's trigger payload: a JSON
/// object, or none.
fn child_payload(task: &str) -> std::result::Result<serde_json::Value, String> {
    let task = task.trim();
    if task.is_empty() {
        return Ok(serde_json::json!({}));
    }
    match serde_json::from_str::<serde_json::Value>(task) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => Err("task must resolve to a JSON object (the child's payload)".into()),
        Err(e) => Err(format!("task is not valid JSON (the child's payload): {e}")),
    }
}

/// What a finished child run hands its calling step: how it ended (the
/// outcome only when a branch claims that status, so an unclaimed one falls
/// through to `goto` like an unsure decision), why if it did not complete,
/// every output its steps produced (the step that ran last wins a name) and
/// the full per-step map under `steps`.
fn child_run_output(
    row: &WorkflowInstanceRow,
    workflow_id: &str,
    branches: &[String],
) -> serde_json::Value {
    let vars: serde_json::Value = serde_json::from_str(&row.variables).unwrap_or_default();
    let steps = vars.get("steps").cloned().unwrap_or(serde_json::json!({}));
    let history: Vec<serde_json::Value> =
        serde_json::from_str(&row.step_history).unwrap_or_default();
    let ran: Vec<&str> = history
        .iter()
        .filter_map(|r| r.get("step_id")?.as_str())
        .collect();
    let mut output = serde_json::Map::new();
    let by_step = steps.as_object();
    let in_run_order = by_step
        .into_iter()
        .flat_map(|m| m.keys().map(String::as_str))
        .filter(|id| !ran.contains(id))
        .chain(ran.iter().copied())
        .filter_map(|id| by_step.and_then(|m| m.get(id)))
        .filter_map(serde_json::Value::as_object)
        .flat_map(|fields| fields.iter());
    for (name, value) in in_run_order {
        output.insert(name.clone(), value.clone());
    }
    output.insert("instance_id".into(), serde_json::json!(row.id));
    output.insert("workflow_id".into(), serde_json::json!(workflow_id));
    output.insert("status".into(), serde_json::json!(row.status));
    if branches.contains(&row.status) {
        output.insert("outcome".into(), serde_json::json!(row.status));
    }
    for key in ["_failure_reason", "_cancellation_reason"] {
        if let Some(reason) = vars.get(key) {
            output.insert(key.trim_start_matches('_').into(), reason.clone());
        }
    }
    output.insert("steps".into(), steps);
    serde_json::Value::Object(output)
}

/// What a coding round books to its step: the attempt that served it plus
/// every attempt a fallback chain handed over from, folded into the step's
/// tokens and cost. A round that errored had no serving attempt, so the
/// engine asked for is named.
fn round_booking(
    base: StepProvenance,
    asked: &str,
    result: &Result<coding_engine::CliRoundResult>,
) -> (StepProvenance, f64) {
    let (served, handed_over) = match result {
        Ok(r) => (
            Some(coding_engine::attempt_of(asked, r)),
            r.handed_over.clone(),
        ),
        Err(e) => (None, coding_engine::handed_over(e)),
    };
    let (input_tokens, output_tokens, cost_usd) =
        served
            .iter()
            .chain(&handed_over)
            .fold((0, 0, 0.0), |(input, output, cost), a| {
                (
                    input + a.input_tokens,
                    output + a.output_tokens,
                    cost + a.cost_usd,
                )
            });
    let engine = served
        .as_ref()
        .map_or_else(|| asked.to_string(), |s| s.engine.clone());
    (
        StepProvenance {
            engine: Some(engine),
            model: served.and_then(|s| s.model),
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            handed_over,
            ..base
        },
        cost_usd,
    )
}

/// The step a run is on, or the last one it ran.
fn current_or_last_step(instance: &WorkflowInstance) -> Option<&str> {
    instance
        .current_step_id
        .as_deref()
        .or_else(|| instance.step_history.last().map(|s| s.step_id.as_str()))
}

/// The message a halt posts: which workflow, step and instance it was, the
/// definition's own `failure_report` (the ticket, the PR) when it has one,
/// then what went wrong. A human reading the channel must be able to tell
/// which piece of work stopped without opening the hub.
/// The end of a failed run's narration, quoted: the last lines say what went
/// wrong, the first say what it set out to do.
fn narration_tail(text: &str) -> String {
    const KEEP: usize = 600;
    let text = text.trim();
    let count = text.chars().count();
    let tail: String = text.chars().skip(count.saturating_sub(KEEP)).collect();
    let tail = if count > KEEP {
        format!("…{tail}")
    } else {
        tail
    };
    tail.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn failure_card(
    meta: Option<&WorkflowMeta>,
    instance: &WorkflowInstance,
    reason: &str,
    body: &str,
) -> String {
    let name = meta.map_or(instance.workflow_id.as_str(), |m| m.name.as_str());
    let step = current_or_last_step(instance).unwrap_or("unknown");
    let short_id: String = instance.id.chars().take(8).collect();
    let header = format!("⚠️ *{name}* halted at step '{step}' (instance {short_id})");
    let context = meta
        .and_then(|m| m.failure_report.as_deref())
        .and_then(|template| {
            let mut variables = instance.variables.as_object().cloned().unwrap_or_default();
            variables.insert("step".into(), step.into());
            variables.insert("error".into(), reason.into());
            let variables = serde_json::Value::Object(variables);
            workforce_orchestrator::workflow::report::render_lenient(template, &variables)
                .inspect_err(|e| warn!(workflow = %instance.workflow_id, error = %e, "failure_report did not render"))
                .ok()
        })
        .filter(|text| !text.is_empty());
    [Some(header), context, Some(body.to_string())]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n")
}

impl WorkflowRuntime {
    pub fn new(
        service: Arc<AgentService>,
        telemetry_store: Arc<dyn TelemetryStore>,
        workflows_dir: PathBuf,
    ) -> Self {
        let map: HashMap<String, OwnedDefinition> = match WorkflowDefinition::load_dir(
            &workflows_dir,
        ) {
            Ok(defs) => {
                let count = defs.len();
                info!(count, dir = %workflows_dir.display(), "Loaded workflow definitions");
                defs.into_iter()
                    .map(|d| (d.workflow.id.clone(), system_def(d)))
                    .collect()
            }
            Err(e) => {
                warn!(error = %e, dir = %workflows_dir.display(), "Failed to load workflow definitions");
                HashMap::new()
            }
        };
        let trigger_index = Arc::new(RwLock::new(TriggerIndex::build(&map, &HashMap::new())));
        let definitions = Arc::new(RwLock::new(map));

        Self {
            service,
            definitions,
            telemetry_store,
            workflows_dir,
            surface_registry_override: OnceLock::new(),
            agent_registry: OnceLock::new(),
            agent_surfaces: parking_lot::RwLock::default(),
            cancel_tokens: tokio::sync::Mutex::new(HashMap::new()),
            active_loops: parking_lot::Mutex::new(std::collections::HashSet::new()),
            held_schedules: parking_lot::Mutex::new(std::collections::BTreeSet::new()),
            capacity: tokio::sync::Mutex::new(()),
            shutdown: std::sync::OnceLock::new(),
            verifier: OnceLock::new(),
            decisions: OnceLock::new(),
            worktree_manager: OnceLock::new(),
            remote_launcher: OnceLock::new(),
            coding_engines: OnceLock::new(),
            mcp_exposed_tools: OnceLock::new(),
            store: OnceLock::new(),
            trigger_index,
            live_streams: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Attach the DB definition store. Call before [`Self::reload_definitions`]
    /// so the first load reads from Postgres.
    pub fn set_store(&self, store: Arc<workforce_orchestrator::workflow_store::WorkflowStore>) {
        let _ = self.store.set(store);
    }

    pub(crate) fn def_store(
        &self,
    ) -> Option<&Arc<workforce_orchestrator::workflow_store::WorkflowStore>> {
        self.store.get()
    }

    /// Run a future to completion from a sync method on the multi-thread
    /// runtime (used by the sync source-accessors when reading from the DB).
    fn block_on<F, T>(fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }

    /// Configure the tool patterns Claude Code steps are served over MCP, as
    /// [`McpConfig::step_patterns`] reads them.
    ///
    /// [`McpConfig::step_patterns`]: workforce_common::settings::McpConfig::step_patterns
    pub fn set_mcp_exposed_tools(&self, patterns: Vec<String>) {
        let _ = self.mcp_exposed_tools.set(patterns);
    }

    /// Inject worktree manager for per-workflow-instance repo isolation.
    pub fn set_worktree_manager(&self, mgr: Arc<WorktreeManager>) {
        let _ = self.worktree_manager.set(mgr);
    }

    /// Run `claude-code` steps on the coding worker through `launcher` instead
    /// of in a local worktree.
    pub fn set_remote_launcher(&self, launcher: Arc<dyn SandboxLauncher>) {
        let _ = self.remote_launcher.set(launcher);
    }

    #[cfg(test)]
    fn set_verifier(&self, verifier: Arc<dyn Verifier>) {
        let _ = self.verifier.set(verifier);
    }

    #[cfg(test)]
    fn set_decisions(
        &self,
        decisions: Arc<crate::decide::Decisions>,
        recorder: Option<crate::decision_records::Recorder>,
    ) {
        let _ = self.decisions.set((decisions, recorder));
    }

    /// The install's decision backends, with where their records go.
    fn decisions(&self) -> Option<Decided> {
        self.decisions.get().cloned().or_else(|| {
            let recorder = crate::decision_records::recorder();
            Some((crate::decide::decisions()?, recorder))
        })
    }

    /// Ask the decision model which outcome `exec`'s step should end in,
    /// before its turn, when the step declares `decide_first`. `None` when
    /// it does not, or there is no answer: the turn runs.
    async fn ask_first(
        &self,
        instance: &WorkflowInstance,
        step: Option<&workforce_orchestrator::workflow::definition::StepDefinition>,
        exec: &StepExecution,
        step_service: &Arc<AgentService>,
    ) -> Option<crate::decide::AskedFirst> {
        let (ask, step) = (exec.decide_first.as_ref()?, step?);
        // A turn sent back for correction is the agent's to finish.
        if instance.pending_correction(&exec.step_id).is_some() {
            return None;
        }
        let (decisions, recorder) = self.decisions()?;
        let caller = crate::credentials::Caller::in_scope(
            step_service.vault.clone(),
            step_service.global_vault.clone(),
        );
        let answer = decisions
            .decide_first(&caller, step_service.agent_decision().as_ref(), step, ask)
            .await?;
        let subject = crate::decision_records::Subject {
            instance_id: Some(instance.id.clone()),
            step_id: Some(exec.step_id.clone()),
            ..step_service.decision_subject(crate::user_config::current_identity_id(), "workflow")
        };
        Some(crate::decide::AskedFirst::of(
            answer, ask, step, instance, subject, recorder,
        ))
    }

    /// Run `claude-code` steps on the install's configured engine chain.
    pub fn set_coding_engines(
        &self,
        engines: Arc<coding_engine::Engines>,
        config: CodingEngineConfig,
    ) {
        let _ = self.coding_engines.set((engines, config));
    }

    /// The engine for a step: the engine the step names (`claude-code` keeps
    /// the install's configured primary), over the step's own chain, else the
    /// install's chain, else the primary alone.
    fn coding_engine_for(
        &self,
        exec: &workforce_orchestrator::workflow::engine::StepExecution,
    ) -> Result<Arc<dyn CodingEngine>> {
        let (engines, config) = self.coding_engines.get().cloned().unwrap_or_default();
        let primary = (exec.engine == "codex").then_some(CodingEngineKind::Codex);
        engines.configured(
            &config,
            primary,
            (!exec.fallback.is_empty()).then_some(exec.fallback.as_slice()),
        )
    }

    pub fn set_surface_registry(&self, registry: Arc<tokio::sync::RwLock<SurfaceRegistry>>) {
        let _ = self.surface_registry_override.set(registry);
    }

    /// Inject agent registry for per-step agent routing.
    /// Called after all agent services are built (in main.rs startup).
    pub fn set_agent_registry(&self, registry: Arc<dyn crate::hosts::RegistryView>) {
        let _ = self.agent_registry.set(registry);
    }

    /// Inject per-agent Slack surfaces so workflow messages use the correct bot token.
    /// `agent_bindings` maps workspace_id → agent_id (from Slack config).
    /// `workspace_surfaces` maps workspace_id → surface (from Slack skill start).
    /// We build an inverse map: agent_id → surface.
    pub fn set_agent_surfaces(
        &self,
        agent_bindings: &HashMap<String, String>,
        workspace_surfaces: &HashMap<String, Arc<dyn ChannelSurface>>,
    ) {
        let mut map: HashMap<String, Arc<dyn ChannelSurface>> = HashMap::new();
        for (workspace_id, agent_id) in agent_bindings {
            if let Some(surface) = workspace_surfaces.get(workspace_id) {
                map.insert(agent_id.clone(), Arc::clone(surface));
            }
        }
        info!(agents = ?map.keys().collect::<Vec<_>>(), "Configured per-agent Slack surfaces for workflows");
        *self.agent_surfaces.write() = map;
    }

    pub async fn reload_definitions(&self) -> Result<usize> {
        let (map, endpoints): (HashMap<String, OwnedDefinition>, _) = match self.def_store() {
            // DB is the source of truth. Skip rows that no longer parse rather
            // than failing the whole reload (mirrors `load_dir`'s leniency).
            Some(store) => {
                let (rows, known) = store.list_with_endpoints(KIND_WORKFLOW).await?;
                let map: HashMap<String, OwnedDefinition> =
                    rows.into_iter().filter_map(stored_to_owned).collect();
                let minted = store
                    .ensure_endpoints(&known, &trigger_bindings(&map))
                    .await;
                let endpoints = match minted {
                    Ok(endpoints) => endpoints,
                    // Never `?` this: the definitions must still be published,
                    // or every trigger — including the legacy paths external
                    // callers already hold — starts 404ing. Fall back to what
                    // was already minted rather than to nothing: a failure that
                    // only a newly declared trigger provoked must not take every
                    // other minted address off the air until the next reload,
                    // which only a definition edit will bring about.
                    Err(e) => {
                        warn!(error = %e, "Minting stable trigger paths failed — serving the paths already minted");
                        known
                    }
                };
                (map, endpoints)
            }
            None => (
                WorkflowDefinition::load_dir(&self.workflows_dir)?
                    .into_iter()
                    .map(|d| (d.workflow.id.clone(), system_def(d)))
                    .collect(),
                HashMap::new(),
            ),
        };
        for (key, owned) in map.iter() {
            let issues = owned.def.authoring_issues();
            if !issues.is_empty() {
                warn!(
                    workflow = %key,
                    issues = %issues.join("; "),
                    "Workflow has unresolvable template references — it will fail on the paths that hit them"
                );
            }
        }

        let count = map.len();
        *self.trigger_index.write().await = TriggerIndex::build(&map, &endpoints);
        *self.definitions.write().await = map;
        info!(count, "Reloaded workflow definitions");
        if let Err(e) = self.sync_schedule_triggers().await {
            warn!(error = %e, "Failed to sync workflow schedule triggers after reload");
        }
        Ok(count)
    }

    /// Reconcile the scheduler's `workflow-schedule:*` jobs with the
    /// `TriggerDefinition::Schedule` entries on currently loaded workflow
    /// definitions. Registration is by job name, so repeated and concurrent
    /// calls converge on one job per trigger instead of accumulating a fresh
    /// row each time. Safe to call repeatedly (startup, hot-reload, explicit
    /// `save`). Returns the number of schedule triggers registered.
    pub async fn sync_schedule_triggers(&self) -> Result<usize> {
        let Some(scheduler) = self.service.scheduler.as_ref() else {
            return Ok(0);
        };
        // Registering before the registry is wired would arm a cron that cannot
        // resolve its step agent. The hub syncs explicitly once wiring is done.
        if self.agent_registry.get().is_none() {
            debug!("Schedule sync deferred: agent registry not wired yet");
            return Ok(0);
        }

        let snapshot: Vec<(String, OwnedDefinition)> = self
            .definitions
            .read()
            .await
            .iter()
            .map(|(key, owned)| (key.clone(), owned.clone()))
            .collect();
        let mut registrations = Vec::new();
        let mut held = std::collections::BTreeSet::new();
        for (workflow_id, owned) in &snapshot {
            for (idx, trigger) in owned.def.triggers.iter().enumerate() {
                match self
                    .schedule_registration(workflow_id, owned, idx, trigger)
                    .await
                {
                    Registration::Armed(reg) => registrations.push(*reg),
                    Registration::Held(person) => {
                        held.insert(person);
                    }
                    Registration::Skipped => {}
                }
            }
        }

        let mut live_ids: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(registrations.len());
        for reg in &registrations {
            match scheduler
                .upsert_job(
                    &reg.name,
                    Some(&reg.description),
                    reg.schedule.clone(),
                    reg.payload.clone(),
                )
                .await
            {
                Ok(job_id) => {
                    scheduler
                        .update_job_run_as(&job_id, reg.identity.clone())
                        .await;
                    live_ids.insert(job_id);
                }
                Err(e) => warn!(job = %reg.name, error = %e, "Skipping schedule trigger"),
            }
        }

        *self.held_schedules.lock() = held;

        let jobs = scheduler.list_jobs().await;
        let stale = stale_schedule_job_ids(
            jobs.iter().map(|job| (job.id.as_str(), job.name.as_str())),
            &live_ids,
        );
        for job_id in &stale {
            scheduler.remove_job(job_id).await;
        }

        info!(
            count = live_ids.len(),
            swept = stale.len(),
            "Synced workflow schedule triggers"
        );
        Ok(live_ids.len())
    }

    /// Whether this runtime can arm cron jobs at all. False when it is bound to
    /// a service built without a scheduler — such a runtime silently registers
    /// nothing, so the hub must never adopt one as its live runtime.
    pub fn can_schedule(&self) -> bool {
        self.service.scheduler.is_some()
    }

    /// The agent service that runs a step. A definition that names an agent runs
    /// as that agent or not at all: silently falling back to whichever service
    /// owns this runtime is how a workflow ends up acting under another agent's
    /// identity, including its Slack bot token.
    fn resolve_step_service(
        &self,
        declared: Option<&str>,
    ) -> std::result::Result<(Arc<AgentService>, String), String> {
        // A declared agent is binding. An undeclared one resolves to the registry
        // default, falling back to this runtime's own service where no registry
        // is wired (embedded and test contexts) — that path can misroute nothing,
        // because the definition named nobody.
        let service = match (declared, self.agent_registry.get()) {
            (Some(agent_id), Some(registry)) => registry.get(agent_id).ok_or_else(|| {
                format!("names agent '{agent_id}', which is not a registered agent")
            })?,
            (Some(agent_id), None) => {
                return Err(format!(
                    "names agent '{agent_id}', but no agent registry is wired"
                ))
            }
            (None, Some(registry)) => registry
                .default_agent()
                .unwrap_or_else(|| Arc::clone(&self.service)),
            (None, None) => Arc::clone(&self.service),
        };
        let agent_id = service.agent_id.as_str().to_string();
        // A step runs as the run's identity, so it may use only an agent a
        // chat as them could: a workflow is no way round an agent's access
        // policy. Refused as an unregistered one is, so a run can't learn
        // which agents exist. A service with no gate wired (embedded and test
        // contexts) has no policy to keep.
        if let Some(identity) = crate::user_config::current_identity_id() {
            let admitted = service.access_gate.read().as_ref().is_none_or(|gate| {
                matches!(
                    gate.check(crate::access_gate::AccessRequest {
                        agent_id: &agent_id,
                        identity_id: Some(&identity),
                        surface_ids: &[],
                        is_external: false,
                    }),
                    workforce_common::settings::AccessDecision::Allow
                )
            });
            if !admitted {
                tracing::info!(agent = %agent_id, identity = %identity, "workflow step refused: the agent's access policy does not admit the run's identity");
                return Err(match declared {
                    Some(agent_id) => {
                        format!("names agent '{agent_id}', which is not a registered agent")
                    }
                    None => "runs on the default agent, which this run may not use".to_string(),
                });
            }
        }
        Ok((service, agent_id))
    }

    /// The scheduler job one `TriggerDefinition::Schedule` should map to:
    /// none when the trigger must not run (see [`Self::run_block`]) or its
    /// cadence is below the budget floor, and held when it is the person it
    /// runs as who is suspended.
    async fn schedule_registration(
        &self,
        workflow_id: &str,
        owned: &OwnedDefinition,
        idx: usize,
        trigger: &TriggerDefinition,
    ) -> Registration {
        let TriggerDefinition::Schedule {
            cron,
            tz,
            run_as,
            payload,
        } = trigger
        else {
            return Registration::Skipped;
        };

        let identity = derive_execution_identity(
            owned.owner.as_deref(),
            owned.run_as.as_deref(),
            true,
            run_as.clone(),
        );
        if let Some(block) = self.run_block(owned, identity.as_deref()).await {
            warn!(workflow_id, %block, "Skipping schedule");
            return match (block, identity) {
                (AutomatedRunBlock::Inactive, Some(person))
                    if self.service.suspended(&person).is_some() =>
                {
                    Registration::Held(person)
                }
                _ => Registration::Skipped,
            };
        }

        let schedule = crate::hub_core::schedules::Schedule::Cron {
            expr: cron.clone(),
            tz: tz.clone(),
        };
        // The trigger cron is user-authored workflow data, so it gets the same
        // floor as owned schedules even though registration itself is
        // creator-less (boot/hot-reload context).
        let budget = &self.service.config.budget;
        if budget.enabled {
            if let Err(e) = crate::scheduler::validate_schedule(
                &schedule,
                budget.min_recurring_interval_secs,
                &chrono::Utc::now(),
                self.service.config.user_timezone(),
            ) {
                warn!(workflow_id, error = %e, "Skipping schedule trigger");
                return Registration::Skipped;
            }
        }

        Registration::Armed(Box::new(ScheduleRegistration {
            name: format!("{SCHEDULE_JOB_PREFIX}{workflow_id}:{idx}"),
            description: format!("Scheduled trigger for workflow {workflow_id}"),
            schedule,
            payload: crate::hub_core::schedules::JobPayload::WorkflowStart {
                workflow_id: workflow_id.to_string(),
                payload: payload.clone(),
                channel_id: trigger_delivery_channel(payload),
                thread_id: None,
            },
            identity,
        }))
    }

    pub async fn list_definitions(&self) -> Vec<WorkflowSummary> {
        let mut summaries: Vec<WorkflowSummary> = self
            .definitions
            .read()
            .await
            .values()
            .map(|o| WorkflowSummary {
                // The composite `{owner}/{id}` key is an internal addressing
                // detail; the API surfaces the bare id plus the owner so the
                // editor/run endpoints never carry a `/` in a path segment.
                id: o.def.workflow.id.clone(),
                name: o.def.workflow.name.clone(),
                description: o.def.workflow.description.clone(),
                step_count: o.def.steps.len(),
                owner: o.owner.clone(),
                visibility: o.visibility,
                run_as: o.run_as.clone(),
                automated_runs_blocked: None,
            })
            .collect();
        summaries.sort_by(|a, b| (&a.id, &a.owner).cmp(&(&b.id, &b.owner)));
        summaries
    }

    /// `summaries` with `automated_runs_blocked` evaluated. This reads shares
    /// and identities, so it is for API and tool listings, not the per-turn
    /// prompt that [`Self::list_definitions_visible`] also feeds.
    pub async fn annotate_automation(
        &self,
        summaries: Vec<WorkflowSummary>,
    ) -> Vec<WorkflowSummary> {
        let mut annotated = Vec::with_capacity(summaries.len());
        for mut summary in summaries {
            let owned = self
                .definitions
                .read()
                .await
                .get(&def_key(summary.owner.as_deref(), &summary.id))
                .cloned();
            if let Some(owned) = owned {
                summary.automated_runs_blocked =
                    self.automation_block(&owned).await.map(|b| b.to_string());
            }
            annotated.push(summary);
        }
        annotated
    }

    /// The first reason any of the definition's triggers must not fire, each
    /// judged as the identity it would run as.
    async fn automation_block(&self, owned: &OwnedDefinition) -> Option<AutomatedRunBlock> {
        for trigger in &owned.def.triggers {
            let trigger_run_as = match trigger {
                TriggerDefinition::Schedule { run_as, .. } => run_as.clone(),
                _ => None,
            };
            let identity = derive_execution_identity(
                owned.owner.as_deref(),
                owned.run_as.as_deref(),
                true,
                trigger_run_as,
            );
            if let Some(block) = self.run_block(owned, identity.as_deref()).await {
                return Some(block);
            }
        }
        None
    }

    /// Definitions `listing` reaches: an admin sees all; everyone else sees
    /// global definitions, the ones a team of theirs reaches, their own, and
    /// any shared to them.
    pub async fn list_definitions_visible(&self, listing: &DefListing) -> Vec<WorkflowSummary> {
        self.list_definitions()
            .await
            .into_iter()
            .filter(|s| listing.sees(s.owner.as_deref(), s.visibility, &s.id))
            .collect()
    }

    /// Resolve a `(bare id, owner)` pair to the loaded definition's `(owner,
    /// visibility)`, if present. `owner == None` addresses a system/disk
    /// definition (bare key); `Some` addresses a member definition.
    pub async fn definition_ownership(
        &self,
        bare_id: &str,
        owner: Option<&str>,
    ) -> Option<(Option<String>, Visibility)> {
        self.definitions
            .read()
            .await
            .get(&def_key(owner, bare_id))
            .map(|o| (o.owner.clone(), o.visibility))
    }

    /// The stored run identity of the addressed definition (`None` = owner).
    pub async fn definition_run_as(&self, bare_id: &str, owner: Option<&str>) -> Option<String> {
        self.definitions
            .read()
            .await
            .get(&def_key(owner, bare_id))
            .and_then(|o| o.run_as.clone())
    }

    pub async fn get_definition(&self, id: &str) -> Option<WorkflowDefinition> {
        self.definitions.read().await.get(id).map(|o| o.def.clone())
    }

    /// Why an automated run of the addressed definition (by composite key) as
    /// `run_identity` must not fire now, `None` when it may. The fire-time
    /// companion to the registration-time skip in [`Self::sync_schedule_triggers`],
    /// for callers (the scheduler) that reach [`Self::start_workflow`] directly.
    pub async fn automated_run_block(
        &self,
        key: &str,
        run_identity: Option<&str>,
    ) -> Option<AutomatedRunBlock> {
        let owned = self.definitions.read().await.get(key).cloned();
        match owned {
            Some(owned) => self.run_block(&owned, run_identity).await,
            None => Some(AutomatedRunBlock::Unknown),
        }
    }

    async fn run_block(
        &self,
        owned: &OwnedDefinition,
        run_identity: Option<&str>,
    ) -> Option<AutomatedRunBlock> {
        let owner = owned.owner.as_deref();
        if auto_run_tainted(owner, owned.last_editor.as_deref(), run_identity) {
            return Some(AutomatedRunBlock::Tainted);
        }
        let shared = match (owner, run_identity) {
            (Some(owner), Some(identity)) if identity != owner => {
                self.manages_for_automation(&owned.def.workflow.id, owner, identity)
                    .await
            }
            _ => true,
        };
        let active = run_identity.is_none_or(|uid| self.owner_is_active(uid));
        automated_run_block_reason(
            owner,
            owned.last_editor.as_deref(),
            run_identity,
            shared,
            active,
        )
    }

    /// Whether `identity` may still drive an automated run of a definition
    /// somebody else owns: a live `manage` grant, or the administrator's
    /// standing that let them write that row in the first place
    /// ([`crate::def_write_auth`]). Without the second, an administrator who
    /// fixed an absent owner's workflow could only hand it back to the identity
    /// that is absent — the taint the fix itself stamped would never clear.
    async fn manages_for_automation(&self, id: &str, owner: &str, identity: &str) -> bool {
        crate::authz::is_admin(&self.service, identity)
            || self
                .holds_manage_share(id, owner, identity)
                .await
                .unwrap_or(true)
    }

    /// Whether `identity` may still manage the definition through a share.
    /// `None` when that cannot be judged — no definition store, the team store
    /// not yet bound at boot, or a store error — so the caller fails open the
    /// way `owner_is_active` and the concurrency guard do: a lookup outage
    /// must not delete schedules or count as revoked access.
    async fn holds_manage_share(&self, id: &str, owner: &str, identity: &str) -> Option<bool> {
        let store = self.def_store()?;
        let team_store = self.service.team_store.read().clone()?;
        let teams = TeamReach::new(Some(team_store), Some(identity.to_string()));
        match store
            .caller_share_permission(KIND_WORKFLOW, id, owner, Some(identity), teams.team_ids())
            .await
        {
            Ok(permission) => Some(permission.is_some_and(|p| p.allows(GrantLevel::Manage))),
            Err(e) => {
                warn!(workflow = %id, error = %e, "Share lookup failed; not treating access as revoked");
                None
            }
        }
    }

    pub async fn start_workflow(
        self: &Arc<Self>,
        workflow_id: &str,
        mut payload: serde_json::Value,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
        parent_instance_id: Option<String>,
    ) -> Result<String> {
        let owned = self
            .definitions
            .read()
            .await
            .get(workflow_id)
            .cloned()
            .ok_or_else(|| eyre!("Unknown workflow: '{}'", workflow_id))?;
        let def = &owned.def;

        // Backstop the per-owner concurrency cap on every start path (scheduler,
        // Slack, API), not only the gateway run endpoint. System ids are exempt.
        if self.owner_run_quota_exceeded(owner_of_key(workflow_id)) {
            return Err(eyre!(
                "owner concurrency limit reached ({}); try again once runs complete",
                self.max_concurrent_runs_per_owner()
            ));
        }

        validate_required_context(def, &payload)?;
        self.prefetch_ticket_data(&mut payload, originating_agent_id.as_deref())
            .await?;

        let trigger = TriggerInfo {
            source: TriggerSource::Api,
            payload,
        };

        let principal = crate::user_config::current_principal();
        if let Some(person) = principal
            .run_as
            .as_deref()
            .and_then(|person| self.service.suspended(person))
        {
            return Err(eyre!("{}", crate::standing::refusal(&person)));
        }
        let mut instance = WorkflowExecutor::start(def, trigger, parent_instance_id)?;
        // Address by the composite key (owner-namespaced for member definitions)
        // so re-lookups during execution resolve the right owner's definition.
        instance.workflow_id = workflow_id.to_string();
        let instance_id = instance.id.clone();
        persist_principal(&mut instance, &principal);
        persist_steering(&mut instance, &owned);
        persist_delivery_context(
            &mut instance,
            channel_id.as_deref(),
            thread_id.as_deref(),
            workforce_agent::runner::current_desk()
                .map(|scope| scope.target)
                .as_ref(),
        );

        self.save_instance(&instance)?;

        info!(
            instance_id = %instance_id,
            workflow_id = %workflow_id,
            "Starting workflow"
        );

        // Preserve the caller's principal across the detached task — a bare
        // spawn drops the task-locals the scheduler/interactive callers set.
        self.spawn_execute_loop(
            instance_id.clone(),
            channel_id,
            thread_id,
            originating_agent_id,
            principal,
        );

        Ok(instance_id)
    }

    /// Start a workflow with an explicit trigger source (used by webhook and
    /// fire-and-forget HTTP triggers). When `run_as` is set, the execution is
    /// scoped to that identity so per-user credential and memory resolution use
    /// the caller's identity.
    ///
    /// A start that finds the workflow at its `max_concurrent` is not dropped
    /// the way a schedule slot is — the event (a merged PR, a deploy) would be
    /// lost — but saved as `queued` and promoted FIFO by [`Self::promote_queued`]
    /// once a sibling finishes. The returned id is the instance either way.
    pub async fn start_workflow_from_trigger(
        self: &Arc<Self>,
        workflow_id: &str,
        trigger: TriggerInfo,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
        run_as: Option<String>,
    ) -> Result<String> {
        let owned = self
            .definitions
            .read()
            .await
            .get(workflow_id)
            .cloned()
            .ok_or_else(|| eyre!("Unknown workflow: '{}'", workflow_id))?;
        let identity = derive_execution_identity(
            owned.owner.as_deref(),
            owned.run_as.as_deref(),
            true,
            run_as,
        );
        if let Some(block) = self.run_block(&owned, identity.as_deref()).await {
            return Err(eyre!(
                "workflow '{}' cannot run automatically: {block}",
                workflow_id
            ));
        }
        self.admit_start(
            workflow_id,
            &owned,
            trigger,
            RunLaunch {
                channel_id,
                thread_id,
                originating_agent_id,
                identity,
                parent_instance_id: None,
            },
        )
        .await
    }

    /// Save an instance of `def` under `workflow_id` and run it, or hold it
    /// as `queued` when the workflow is at `max_concurrent` (promoted FIFO
    /// once a sibling finishes). The returned id is the instance either way.
    async fn admit_start(
        self: &Arc<Self>,
        workflow_id: &str,
        owned: &OwnedDefinition,
        trigger: TriggerInfo,
        launch: RunLaunch,
    ) -> Result<String> {
        let def = &owned.def;
        let RunLaunch {
            channel_id,
            thread_id,
            originating_agent_id,
            identity,
            parent_instance_id,
        } = launch;
        let mut instance = WorkflowExecutor::start(def, trigger, parent_instance_id)?;
        instance.workflow_id = workflow_id.to_string();
        let instance_id = instance.id.clone();
        let principal = launch_principal(identity);
        persist_principal(&mut instance, &principal);
        persist_steering(&mut instance, owned);
        persist_delivery_context(
            &mut instance,
            channel_id.as_deref(),
            thread_id.as_deref(),
            None,
        );

        let cap = def.workflow.max_concurrent;
        let _admission = self.capacity.lock().await;
        // A store error admits rather than discards: the event behind a trigger
        // (a merged PR, a deploy) is not reproducible, so a counting hiccup must
        // not be the thing that loses it. Same call the scheduler makes.
        let active = self.active_instance_ids(workflow_id).unwrap_or_else(|e| {
            warn!(
                workflow_id = %workflow_id,
                error = %e,
                "Concurrency check failed — admitting the trigger start anyway"
            );
            Vec::new()
        });
        if blocking(&active, cap).is_some() {
            instance.status = InstanceStatus::Queued;
            self.save_instance(&instance)?;
            info!(
                instance_id = %instance_id,
                workflow_id = %workflow_id,
                max_concurrent = cap,
                "Workflow at max_concurrent — queued trigger start"
            );
            return Ok(instance_id);
        }

        self.save_instance(&instance)?;

        info!(
            instance_id = %instance_id,
            workflow_id = %workflow_id,
            "Starting workflow from trigger"
        );

        self.spawn_execute_loop(
            instance_id.clone(),
            channel_id,
            thread_id,
            originating_agent_id,
            principal,
        );

        Ok(instance_id)
    }

    /// Run a saved `running` instance's executor on a detached task under
    /// `identity`, reporting a loop error as the run's terminal status. The
    /// one spawn path for every fresh start — interactive, trigger, and queue
    /// promotion — so they cannot drift apart.
    fn spawn_execute_loop(
        self: &Arc<Self>,
        instance_id: String,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
        principal: TurnPrincipal,
    ) {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = runtime.run_scoped(
                principal,
                runtime.execute_loop(
                    &instance_id,
                    channel_id.clone(),
                    thread_id.clone(),
                    originating_agent_id,
                ),
            );
            if let Err(e) = outcome.await {
                let err_text = format!("{e}");
                error!(instance_id = %instance_id, error = %err_text, "Workflow execution failed");
                runtime
                    .post_terminal_status(
                        &instance_id,
                        channel_id.as_deref(),
                        thread_id.as_deref(),
                        TerminalOutcome::Failed { error: &err_text },
                    )
                    .await;
            }
        });
    }

    /// Fill the free `max_concurrent` slots of `workflow_id` from its queue,
    /// oldest first, starting each promoted instance's executor. Called after
    /// every terminal transition and at boot; a no-op when nothing is queued
    /// or no slot is free. Promotion is a side effect of finishing a run,
    /// never a reason to fail one, so a store error is logged, not returned.
    pub async fn promote_queued(self: &Arc<Self>, workflow_id: &str) {
        let _admission = self.capacity.lock().await;
        if let Err(e) = self.fill_free_slots(workflow_id).await {
            warn!(workflow_id, error = %e, "Failed to promote queued workflow instances");
        }
    }

    async fn fill_free_slots(self: &Arc<Self>, workflow_id: &str) -> Result<()> {
        let cap = self.max_concurrent(workflow_id).await;
        let mut active = self.active_instance_ids(workflow_id)?;
        for queued in self.queued_instances(workflow_id)? {
            if blocking(&active, cap).is_some() {
                break;
            }
            let promoted = self.update_instance(&queued.id, |instance| {
                Ok::<_, eyre::Report>((instance.status == InstanceStatus::Queued).then(|| {
                    instance.status = InstanceStatus::Running;
                    instance.updated_at = Utc::now();
                    instance.clone()
                }))
            })?;
            let Some(instance) = promoted else {
                continue;
            };
            info!(
                instance_id = %instance.id,
                workflow_id,
                "Promoted queued workflow instance"
            );
            let (channel_id, thread_id) = delivery_context(&instance);
            self.spawn_execute_loop(
                instance.id.clone(),
                channel_id,
                thread_id,
                instance.agent_id.clone(),
                instance_principal(&instance),
            );
            active.push(instance.id);
        }
        Ok(())
    }

    /// Promote from the queue of every workflow that has one.
    async fn promote_all_queued(self: &Arc<Self>) {
        let workflow_ids: std::collections::BTreeSet<String> =
            match self.list_instances(Some("queued"), None) {
                Ok(rows) => rows.into_iter().map(|row| row.workflow_id).collect(),
                Err(e) => {
                    warn!(error = %e, "Failed to query queued workflow instances");
                    return;
                }
            };
        for workflow_id in &workflow_ids {
            self.promote_queued(workflow_id).await;
        }
    }

    /// The instances of `workflow_id` holding for a slot, oldest first.
    fn queued_instances(&self, workflow_id: &str) -> Result<Vec<WorkflowInstance>> {
        // A row that vanished or cannot be parsed is skipped, not propagated:
        // one bad row would otherwise wedge promotion for the whole workflow.
        let mut queued: Vec<WorkflowInstance> = self
            .list_instances(Some("queued"), Some(workflow_id))?
            .iter()
            .filter_map(|row| match self.load_instance(&row.id) {
                Ok(instance) => Some(instance),
                Err(e) => {
                    warn!(instance_id = %row.id, error = %e, "Skipping unloadable queued instance");
                    None
                }
            })
            .collect();
        queued.sort_by_key(|instance| instance.created_at);
        Ok(queued)
    }

    async fn max_concurrent(&self, workflow_id: &str) -> u32 {
        self.definitions
            .read()
            .await
            .get(workflow_id)
            .map(|o| o.def.workflow.max_concurrent)
            .unwrap_or(1)
    }

    /// Whether this workflow asked to be held rather than failed when the
    /// subscription's usage window is spent. A workflow whose definition has
    /// gone (deleted mid-run) keeps the default: fail, so nothing is parked
    /// with no definition left to resume it under.
    async fn pauses_on_usage_cap(&self, workflow_id: &str) -> bool {
        self.definitions
            .read()
            .await
            .get(workflow_id)
            .map(|o| o.def.workflow.pause_on_usage_cap)
            .unwrap_or(false)
    }

    /// Resolve an inbound public trigger path — minted or owner-derived — to
    /// its `(workflow_id, trigger)`. The trigger is guaranteed to be a
    /// `TriggerDefinition::Http`.
    ///
    /// Resolution needs both an index entry and a currently-loaded definition,
    /// which is what makes a deleted definition's minted path 404 while its
    /// row waits for the definition to come back.
    pub async fn resolve_http_trigger(
        &self,
        public_path: &str,
    ) -> Option<(String, TriggerDefinition)> {
        let route = self.trigger_index.read().await.route(public_path)?;
        let defs = self.definitions.read().await;
        let trigger =
            defs.get(&route.def_key)?.def.triggers.iter().find(
                |t| matches!(t, TriggerDefinition::Http { id, .. } if *id == route.trigger_id),
            )?;
        Some((route.def_key, trigger.clone()))
    }

    /// Both public URLs of each HTTP trigger on `def_key`, absolute against
    /// `[hub].public_url` and carrying the placeholder base while it is unset.
    pub async fn trigger_urls(&self, def_key: &str) -> Vec<TriggerUrls> {
        let base = public_base_url(self.service.config.hub.public_url.as_deref());
        self.trigger_index
            .read()
            .await
            .paths_for(def_key)
            .iter()
            .map(|paths| trigger_urls_of(base, paths))
            .collect()
    }

    /// `entry` carrying the definition's trigger URLs, or unchanged where it
    /// declares no HTTP trigger — a surface reports the field or nothing,
    /// never an empty list.
    pub async fn with_trigger_urls(
        &self,
        def_key: &str,
        mut entry: serde_json::Value,
    ) -> serde_json::Value {
        let urls = self.trigger_urls(def_key).await;
        if !urls.is_empty() {
            entry["trigger_urls"] = serde_json::json!(urls);
        }
        entry
    }

    /// One list entry, as `GET /api/workflows` and the `workflow` tool's
    /// `list` both report it. A summary carries the bare id and the owner
    /// separately, so the addressing key has to be rebuilt to reach the index.
    async fn summary_json(&self, summary: &WorkflowSummary) -> serde_json::Value {
        self.with_trigger_urls(
            &def_key(summary.owner.as_deref(), &summary.id),
            serde_json::json!({
                "id": summary.id,
                "name": summary.name,
                "description": summary.description,
                "steps": summary.step_count,
                "owner": summary.owner,
                "visibility": summary.visibility,
                "run_as": summary.run_as,
                "automated_runs_blocked": summary.automated_runs_blocked,
            }),
        )
        .await
    }

    /// Every list entry of `summaries`, so the two listing surfaces share one
    /// renderer rather than each walking the summaries themselves.
    pub async fn summaries_json(&self, summaries: &[WorkflowSummary]) -> Vec<serde_json::Value> {
        let mut entries = Vec::with_capacity(summaries.len());
        for summary in summaries {
            entries.push(self.summary_json(summary).await);
        }
        entries
    }

    /// Start a workflow and return a receiver streaming its step-lifecycle
    /// events, ending with a `Completed`/`Failed` event. The receiver is
    /// subscribed before the executing task is spawned, so no early event is
    /// lost. When `run_as` is set the execution is scoped to that identity.
    pub async fn start_workflow_streaming(
        self: &Arc<Self>,
        workflow_id: &str,
        trigger: TriggerInfo,
        run_as: Option<String>,
    ) -> Result<(
        String,
        tokio::sync::broadcast::Receiver<WorkflowStreamEvent>,
    )> {
        let owned = self
            .definitions
            .read()
            .await
            .get(workflow_id)
            .cloned()
            .ok_or_else(|| eyre!("Unknown workflow: '{}'", workflow_id))?;
        let identity = derive_execution_identity(
            owned.owner.as_deref(),
            owned.run_as.as_deref(),
            true,
            run_as,
        );
        if let Some(block) = self.run_block(&owned, identity.as_deref()).await {
            return Err(eyre!(
                "workflow '{}' cannot run automatically: {block}",
                workflow_id
            ));
        }

        let mut instance = WorkflowExecutor::start(&owned.def, trigger, None)?;
        instance.workflow_id = workflow_id.to_string();
        let instance_id = instance.id.clone();
        let principal = launch_principal(identity);
        persist_principal(&mut instance, &principal);
        persist_steering(&mut instance, &owned);
        self.save_instance(&instance)?;

        let (tx, rx) = tokio::sync::broadcast::channel(256);
        self.live_streams
            .write()
            .await
            .insert(instance_id.clone(), tx);

        info!(
            instance_id = %instance_id,
            workflow_id = %workflow_id,
            "Starting streaming workflow from HTTP trigger"
        );

        let runtime = Arc::clone(self);
        let id = instance_id.clone();
        tokio::spawn(async move {
            let _guard = StreamGuard {
                runtime: Arc::clone(&runtime),
                instance_id: id.clone(),
            };
            let outcome =
                runtime.run_scoped(principal, runtime.execute_loop(&id, None, None, None));
            match outcome.await {
                Ok(()) => {
                    let result = runtime
                        .load_instance(&id)
                        .map(|i| i.variables)
                        .unwrap_or(serde_json::Value::Null);
                    runtime
                        .emit_stream_event(&id, WorkflowStreamEvent::Completed { result })
                        .await;
                }
                Err(e) => {
                    let reason = format!("{e}");
                    error!(instance_id = %id, error = %reason, "Streaming workflow failed");
                    runtime
                        .post_terminal_status(
                            &id,
                            None,
                            None,
                            TerminalOutcome::Failed { error: &reason },
                        )
                        .await;
                    runtime
                        .emit_stream_event(&id, WorkflowStreamEvent::Failed { reason })
                        .await;
                }
            }
        });

        Ok((instance_id, rx))
    }

    /// What a step that runs no model turn answers: `None` when the step is
    /// a model turn after all; else `Ok(Some)` the rendered response,
    /// `Ok(None)` when the run was stopped while its child was in flight and
    /// there is nothing to record, `Err` what halts the run. A loop resumed
    /// here after the answer arrived (the hub was restarted, or this instance
    /// was recovered as orphaned) has it on the instance; the work is not
    /// done again.
    async fn run_delegated_step(
        self: &Arc<Self>,
        instance: &mut WorkflowInstance,
        def: &WorkflowDefinition,
        exec: &StepExecution,
        ctx: DelegatedContext<'_>,
        provenance: &mut StepProvenance,
    ) -> Option<std::result::Result<Option<String>, String>> {
        if !exec.mode.is_delegated() {
            return None;
        }
        if let Some(response) = take_reported_tool_result(instance, &exec.step_id) {
            info!(
                instance_id = %instance.id,
                step = %exec.step_id,
                "Step already answered before the loop was interrupted; reusing its answer"
            );
            return Some(Ok(Some(response)));
        }
        let step_def = def.get_step(&exec.step_id);
        let shape = StepShape {
            declared: step_def
                .map(|s| s.outputs.iter().map(|o| o.name.clone()).collect())
                .unwrap_or_default(),
            branches: step_def
                .map(|s| s.branches.iter().map(|b| b.when.clone()).collect())
                .unwrap_or_default(),
        };
        Some(match &exec.mode {
            StepMode::Tool(tool_name) => self
                .execute_tool_step(
                    tool_name,
                    &exec.task_prompt,
                    &shape.declared,
                    ctx.step_service,
                    (&instance.id, instance.workflow_id.as_str()),
                )
                .await
                .map(Some),
            StepMode::Decide(spec) => {
                let caller = crate::credentials::Caller::in_scope(
                    ctx.step_service.vault.clone(),
                    ctx.step_service.global_vault.clone(),
                );
                self.retry_while_busy(&instance.id, &crate::decide::BUSY_RETRY_WAITS, || {
                    crate::decide::run_decision_step(
                        &caller,
                        spec,
                        &exec.task_prompt,
                        &shape.branches,
                        &shape.declared,
                    )
                })
                .await
                .map(|decided| {
                    decided.map(|decided| {
                        provenance.model = Some(decided.backend);
                        decided.response
                    })
                })
            }
            StepMode::Workflow(call) => {
                self.run_workflow_step(instance, call, exec, &shape, ctx)
                    .await
            }
            StepMode::Agent | StepMode::Report => return None,
        })
    }

    /// Run `call` for the step: start the child (or attach to the one a
    /// previous pass of this step started, so a resumed loop never starts a
    /// second), then either return at once or wait for it to end. The step's
    /// own `timeout_secs` caps the wait; the child keeps running past it.
    async fn run_workflow_step(
        self: &Arc<Self>,
        instance: &mut WorkflowInstance,
        call: &WorkflowCall,
        exec: &StepExecution,
        shape: &StepShape,
        ctx: DelegatedContext<'_>,
    ) -> std::result::Result<Option<String>, String> {
        let child_id = self
            .start_child_run(
                instance,
                &exec.step_id,
                &call.id,
                || child_payload(&exec.task_prompt),
                ChildLaunch {
                    agent_id: ctx.step_agent_id,
                    channel_id: ctx.channel_id,
                    thread_id: ctx.thread_id,
                },
            )
            .await?;
        if !call.wait {
            clear_child_started(instance);
            let mut output = serde_json::json!({
                "instance_id": child_id,
                "workflow_id": call.id,
                "status": CHILD_STARTED_OUTCOME,
            });
            if shape.branches.iter().any(|b| b == CHILD_STARTED_OUTCOME) {
                output["outcome"] = serde_json::json!(CHILD_STARTED_OUTCOME);
            }
            return Ok(Some(render_tool_step_response(&output, &shape.declared)));
        }
        let Some(row) = self
            .await_child(
                &instance.id,
                &child_id,
                exec.timeout_secs.map(Duration::from_secs),
            )
            .await?
        else {
            return Ok(None);
        };
        clear_child_started(instance);
        let output = child_run_output(&row, &call.id, &shape.branches);
        if let Some(halt) = unhandled_child_end(&row, &output, &shape.branches) {
            return Err(format!("workflow '{}' {halt}", call.id));
        }
        Ok(Some(render_tool_step_response(&output, &shape.declared)))
    }

    /// Start `workflow` as a child run of `instance`'s step `step_id`, or
    /// attach to the one a previous pass of that step started, so a resumed
    /// loop never starts a second. `payload` is asked for only when a run is
    /// started. The child's id.
    async fn start_child_run(
        self: &Arc<Self>,
        instance: &mut WorkflowInstance,
        step_id: &str,
        workflow: &str,
        payload: impl FnOnce() -> std::result::Result<serde_json::Value, String>,
        launch: ChildLaunch<'_>,
    ) -> std::result::Result<String, String> {
        let child_key = self
            .resolve_child_key(&instance.workflow_id, workflow)
            .await
            .ok_or_else(|| format!("unknown workflow '{workflow}'"))?;
        let depth = self.call_depth(instance);
        if depth >= MAX_WORKFLOW_CALL_DEPTH {
            return Err(format!(
                "workflow call depth {depth} reached: '{workflow}' would nest deeper than {MAX_WORKFLOW_CALL_DEPTH} runs"
            ));
        }
        if let Some(existing) = self.child_started_by(instance, step_id, &child_key) {
            info!(instance_id = %instance.id, step = %step_id, child = %existing, "Child run already started before the loop was interrupted; attaching to it");
            return Ok(existing);
        }
        let payload = payload()?;
        note_child_started(instance, step_id);
        self.save_instance(instance).map_err(|e| e.to_string())?;
        let started = self
            .start_child_workflow(&child_key, payload, instance, step_id, &launch)
            .await
            .map_err(|e| format!("could not start workflow '{workflow}': {e}"))?;
        note_child_instance(instance, &started);
        self.save_instance(instance).map_err(|e| e.to_string())?;
        info!(instance_id = %instance.id, step = %step_id, child = %started, workflow = %child_key, "Started child workflow");
        Ok(started)
    }

    /// Start `child_key` as a child run of `parent`'s step: the same gates a
    /// trigger start passes (payload against the child's context, the owner
    /// quota, the automated-run block, `max_concurrent` with queueing), under
    /// the parent's identity and delivery context.
    async fn start_child_workflow(
        self: &Arc<Self>,
        child_key: &str,
        mut payload: serde_json::Value,
        parent: &WorkflowInstance,
        step_id: &str,
        launch: &ChildLaunch<'_>,
    ) -> Result<String> {
        let owned = self
            .definitions
            .read()
            .await
            .get(child_key)
            .cloned()
            .ok_or_else(|| eyre!("Unknown workflow: '{child_key}'"))?;
        if self.owner_run_quota_exceeded(owner_of_key(child_key)) {
            return Err(eyre!(
                "owner concurrency limit reached ({}); try again once runs complete",
                self.max_concurrent_runs_per_owner()
            ));
        }
        let identity = current_scoped_user();
        if let Some(block) = self.run_block(&owned, identity.as_deref()).await {
            return Err(eyre!("it cannot run as a step: {block}"));
        }
        validate_required_context(&owned.def, &payload)?;
        self.prefetch_ticket_data(&mut payload, Some(launch.agent_id))
            .await?;
        let trigger = TriggerInfo {
            source: TriggerSource::Workflow {
                instance_id: parent.id.clone(),
                step_id: step_id.to_string(),
            },
            payload,
        };
        self.admit_start(
            child_key,
            &owned,
            trigger,
            RunLaunch {
                channel_id: launch.channel_id.map(str::to_string),
                thread_id: launch.thread_id.map(str::to_string),
                originating_agent_id: Some(launch.agent_id.to_string()),
                identity,
                parent_instance_id: Some(parent.id.clone()),
            },
        )
        .await
    }

    /// The composite key a bare `child` id resolves to from a run of
    /// `parent_key`: the same owner's definition first, then a system one.
    /// Never another member's — an id with an owner in it resolves to nothing.
    async fn resolve_child_key(&self, parent_key: &str, child: &str) -> Option<String> {
        if child.contains('/') {
            return None;
        }
        let definitions = self.definitions.read().await;
        [def_key(owner_of_key(parent_key), child), child.to_string()]
            .into_iter()
            .find(|key| definitions.contains_key(key))
    }

    /// How many runs sit above `instance` in its parent chain.
    fn call_depth(&self, instance: &WorkflowInstance) -> usize {
        std::iter::successors(instance.parent_instance_id.clone(), |id| {
            self.telemetry_store
                .get_workflow_instance(id)
                .ok()
                .flatten()
                .and_then(|row| row.parent_instance_id)
        })
        .take(MAX_WORKFLOW_CALL_DEPTH + 1)
        .count()
    }

    /// The child run a previous pass of `step_id` started: the one the note
    /// names once it has an id, whatever state it is in now; before the id
    /// was noted, a run of `child_key` under this instance since the note.
    fn child_started_by(
        &self,
        instance: &WorkflowInstance,
        step_id: &str,
        child_key: &str,
    ) -> Option<String> {
        let note = instance.variables.get(CHILD_STARTED_KEY)?;
        if note.get("step").and_then(|v| v.as_str()) != Some(step_id) {
            return None;
        }
        let noted = note
            .get("instance_id")
            .and_then(|v| v.as_str())
            .and_then(|id| {
                self.telemetry_store
                    .get_workflow_instance(id)
                    .ok()
                    .flatten()
            })
            .map(|row| row.id);
        if noted.is_some() {
            return noted;
        }
        let since = note.get("since").and_then(|v| v.as_str())?.to_string();
        self.telemetry_store
            .list_child_instances(&instance.id)
            .ok()?
            .into_iter()
            .filter(|row| row.workflow_id == child_key && row.created_at >= since)
            .max_by(|a, b| a.created_at.cmp(&b.created_at))
            .map(|row| row.id)
    }

    /// A decision step's answer, asking again after each of `waits` while
    /// the backends are only busy: a step has no path but its answer, and a
    /// passing overload must not end a long run. Still busy after the last
    /// wait, or failed for any other reason, the step fails with that
    /// reason. `Ok(None)` when this run was stopped while waiting; `Err`
    /// when the process is draining, which recovery resumes.
    async fn retry_while_busy<T, Asked>(
        &self,
        instance_id: &str,
        waits: &[Duration],
        mut ask: impl FnMut() -> Asked,
    ) -> std::result::Result<Option<T>, String>
    where
        Asked: std::future::Future<Output = std::result::Result<T, crate::decide::Undecided>>,
    {
        let token = CancellationToken::new();
        self.cancel_tokens
            .lock()
            .await
            .insert(instance_id.to_string(), token.clone());
        let stopped = || {
            token.is_cancelled()
                || self
                    .telemetry_store
                    .get_workflow_instance(instance_id)
                    .is_ok_and(|row| row.is_some_and(|row| run_ended(&row.status)))
        };
        let mut waits = waits.iter();
        let outcome = loop {
            let undecided = match ask().await {
                Ok(answer) => break Ok(Some(answer)),
                Err(undecided) => undecided,
            };
            if stopped() {
                break Ok(None);
            }
            let Some(wait) = waits.next().filter(|_| undecided.busy) else {
                break Err(undecided.reason);
            };
            if shutdown_cancelled(&self.shutdown) {
                break Err(format!(
                    "hub shutting down while waiting for the decision model: {}",
                    undecided.reason
                ));
            }
            info!(
                instance_id = %instance_id,
                wait_ms = wait.as_millis() as u64,
                reason = %undecided.reason,
                "Decision model busy; the step asks again"
            );
            tokio::select! {
                _ = tokio::time::sleep(*wait) => {}
                _ = token.cancelled() => {}
            }
            if stopped() {
                break Ok(None);
            }
        };
        self.cancel_tokens.lock().await.remove(instance_id);
        outcome
    }

    /// Poll `child_id` until it ends. `Ok(None)` when this run was stopped
    /// meanwhile — by the abort's token or, when the abort landed before the
    /// child existed, by its status — after cancelling the child; `Err` on the
    /// wait's timeout or when the process is draining, which recovery resumes.
    async fn await_child(
        self: &Arc<Self>,
        instance_id: &str,
        child_id: &str,
        timeout: Option<Duration>,
    ) -> std::result::Result<Option<WorkflowInstanceRow>, String> {
        let token = CancellationToken::new();
        self.cancel_tokens
            .lock()
            .await
            .insert(instance_id.to_string(), token.clone());
        let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
        let ended = |id: &str| {
            self.telemetry_store
                .get_workflow_instance(id)
                .map_err(|e| format!("could not read run {id}: {e}"))
                .and_then(|row| row.ok_or_else(|| format!("run {id} disappeared")))
                .map(|row| run_ended(&row.status).then_some(row))
        };
        let outcome = loop {
            match ended(child_id) {
                Err(e) => break Err(e),
                Ok(Some(row)) => break Ok(Some(row)),
                Ok(None) => {}
            }
            let parent_stopped = ended(instance_id).is_ok_and(|row| row.is_some());
            if token.is_cancelled() || parent_stopped {
                self.cascade_abort_children(instance_id).await;
                break Ok(None);
            }
            if shutdown_cancelled(&self.shutdown) {
                break Err(format!(
                    "hub shutting down while waiting for child run {child_id}"
                ));
            }
            if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
                break Err(format!(
                    "child run {child_id} did not finish within {}s; it keeps running",
                    timeout.map(|t| t.as_secs()).unwrap_or_default()
                ));
            }
            tokio::select! {
                _ = tokio::time::sleep(CHILD_POLL_INTERVAL) => {}
                _ = token.cancelled() => {}
            }
        };
        self.cancel_tokens.lock().await.remove(instance_id);
        outcome
    }

    /// Execute a `tool =` workflow step: resolve the input, call the WASM
    /// skill tool, and render its output as a step response the existing
    /// outputs/outcome machinery parses. `Err` is the step-failure text —
    /// for a refused call it carries the platform's remedy.
    async fn execute_tool_step(
        &self,
        tool_name: &str,
        resolved_task: &str,
        declared_outputs: &[String],
        step_service: &Arc<AgentService>,
        (instance_id, workflow_id): (&str, &str),
    ) -> Result<String, String> {
        // Scoped to the skills the executing user may run: a step that names an
        // unknown tool must not answer with everyone else's skill names, and a
        // skill they cannot use is not a step they can run (the skill tool's own
        // gate refuses it either way).
        let installed: Vec<String> = crate::wasm_skill_tools::global_skill_store()
            .map(|store| {
                let caller = match crate::user_config::current_identity_id() {
                    Some(uid) => crate::skill_access::SkillCaller::for_service(step_service, &uid),
                    None => crate::skill_access::SkillCaller::local(None, false),
                };
                caller
                    .visible(&store)
                    .into_iter()
                    .map(|(skill, _)| skill.manifest.name)
                    .collect()
            })
            .unwrap_or_default();
        // Direct dispatch is a skills-only privilege: skill tools carry
        // consent-pinned grants; every other tool still requires an agent turn.
        // The platform exceptions are the document store and the pipelines
        // built on it: their tools carry no credential and check their own
        // scope policy on every call.
        let store_tool = tool_name.starts_with("store.") || tool_name.starts_with("pipeline.");
        if !store_tool
            && !installed
                .iter()
                .any(|skill| tool_name.starts_with(&format!("{skill}.")))
        {
            return Err(format!(
                "'{tool_name}' is not a WASM skill tool you can run — only skill tools may \
                 run as workflow tool steps. Available to you: [{}]",
                installed.join(", ")
            ));
        }
        // The run's identity rides along so a tool can record which run
        // touched what (the document store's claims and lineage need it);
        // a task that sets these itself keeps its own values.
        let mut input = tool_step_input(resolved_task)?;
        if let Some(fields) = input.as_object_mut() {
            fields
                .entry("_instance_id")
                .or_insert_with(|| serde_json::Value::String(instance_id.to_string()));
            fields
                .entry("_workflow_id")
                .or_insert_with(|| serde_json::Value::String(workflow_id.to_string()));
        }
        dispatch_tool_step(
            &step_service.tools,
            step_service.agent_id.as_str(),
            tool_name,
            input,
            declared_outputs,
            step_service.config.tools.validation.mode,
        )
        .await
    }

    /// Install the process shutdown signal (once, at serve wiring).
    pub fn set_shutdown_token(&self, token: tokio_util::sync::CancellationToken) {
        let _ = self.shutdown.set(token);
    }

    /// Whether the process is draining, given `error` from the step that
    /// just failed: a drain already flagged, or one that arrives within
    /// [`RESTART_GRACE`] after the runner closed. A runner lost with no drain
    /// behind it is an ordinary failure.
    async fn interrupted_by_restart(&self, error: &str) -> bool {
        match self.shutdown.get() {
            Some(token) if runner_lost(error) => {
                tokio::time::timeout(RESTART_GRACE, token.cancelled())
                    .await
                    .is_ok()
            }
            Some(token) => token.is_cancelled(),
            None => false,
        }
    }

    /// Run `fut` under `run_as` as `CURRENT_USER_ID` when set, else unscoped.
    async fn run_scoped<F>(&self, principal: TurnPrincipal, fut: F) -> F::Output
    where
        F: std::future::Future,
    {
        crate::user_config::as_principal(&principal, fut).await
    }

    /// The suspended identity `instance` acts as, if it acts as one.
    fn held_person(
        &self,
        instance: &WorkflowInstance,
    ) -> Option<workforce_identity::types::Identity> {
        instance_principal(instance)
            .run_as
            .and_then(|person| self.service.suspended(&person))
    }

    /// Whether `identity_id` names an active identity. Automated runs fail
    /// closed when their owner was suspended or deleted. Returns `true` when no
    /// identity store is wired (single-operator/disk deployments are unaffected).
    fn owner_is_active(&self, identity_id: &str) -> bool {
        let Some(store) = self.service.identity_store.read().clone() else {
            return true;
        };
        match uuid::Uuid::parse_str(identity_id) {
            Ok(uuid) => matches!(
                store.get_by_id(uuid),
                Ok(Some(identity))
                    if identity.status == workforce_identity::types::IdentityStatus::Active
            ),
            Err(_) => false,
        }
    }

    /// Max concurrent running instances per owner, from `[workflows]` config.
    fn max_concurrent_runs_per_owner(&self) -> usize {
        self.service.config.workflows.max_concurrent_runs_per_owner
    }

    /// Whether starting another run for `owner` would exceed the configured
    /// per-owner concurrency cap. System definitions (`None`) are never
    /// throttled. Fail-open on a telemetry error — a quota check never blocks
    /// runs because counting hiccupped.
    pub fn owner_run_quota_exceeded(&self, owner: Option<&str>) -> bool {
        let Some(owner) = owner else {
            return false;
        };
        let ids: Vec<String> = self
            .list_instances(Some("running"), None)
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.workflow_id)
            .collect();
        quota_exceeded(&ids, owner, self.max_concurrent_runs_per_owner())
    }

    /// Build the secret-resolution policy for a run of `owned`, using the
    /// run's executing identity (`CURRENT_USER_ID`).
    fn build_secret_policy(
        &self,
        owned: &OwnedDefinition,
    ) -> workforce_common::secret_policy::SecretPolicy {
        compute_secret_policy(
            owned.owner.as_deref(),
            current_scoped_user(),
            &owned.def.variables,
            WORKFLOW_SHARED_SECRET_PREFIX,
        )
    }

    /// Send a stream event to the instance's streaming caller, if one is
    /// attached. A no-op (with no allocation beyond the event) otherwise.
    async fn emit_stream_event(&self, instance_id: &str, event: WorkflowStreamEvent) {
        if let Some(tx) = self.live_streams.read().await.get(instance_id) {
            let _ = tx.send(event);
        }
    }

    /// Emit a `StepCompleted` event for each step recorded since `emitted` was
    /// last advanced, then advance it. Skips the work entirely when no streaming
    /// caller is attached.
    async fn emit_step_completions(
        &self,
        instance_id: &str,
        instance: &WorkflowInstance,
        emitted: &mut usize,
    ) {
        if *emitted >= instance.step_history.len() {
            return;
        }
        if !self.live_streams.read().await.contains_key(instance_id) {
            *emitted = instance.step_history.len();
            return;
        }
        for record in instance.step_history[*emitted..]
            .iter()
            .filter(|r| !r.failed)
        {
            self.emit_stream_event(
                instance_id,
                WorkflowStreamEvent::StepCompleted {
                    step_id: record.step_id.clone(),
                    report: record.report.clone(),
                    outputs: record.outputs.clone(),
                },
            )
            .await;
        }
        *emitted = instance.step_history.len();
    }

    /// Every loaded definition with the key the runtime addresses it by
    /// (`def_key`), which is what starts a run — not the bare `workflow.id`.
    pub async fn all_definitions_keyed(&self) -> Vec<(String, WorkflowDefinition)> {
        self.definitions
            .read()
            .await
            .iter()
            .map(|(key, o)| (key.clone(), o.def.clone()))
            .collect()
    }

    /// The agent each of `keys` declares, for the loaded definitions that name one.
    pub async fn declared_agents<'a>(
        &self,
        keys: impl IntoIterator<Item = &'a str>,
    ) -> HashMap<String, String> {
        let definitions = self.definitions.read().await;
        keys.into_iter()
            .filter_map(|key| {
                let agent = definitions.get(key)?.def.workflow.agent.clone()?;
                Some((key.to_string(), agent))
            })
            .collect()
    }

    /// Read the raw TOML source of a workflow definition (DB when a store is
    /// attached, else `workflows_dir`).
    pub fn get_definition_source(&self, id: &str, owner: Option<&str>) -> Result<String> {
        if let Some(store) = self.def_store() {
            return Self::block_on(store.get(id, "workflow", owner))?
                .ok_or_else(|| eyre!("Workflow '{}' not found", id));
        }
        let path = self.workflows_dir.join(format!("{}.toml", id));
        std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("Workflow '{}' not found at {}", id, path.display()))
    }

    /// Get raw TOML source for all workflow definitions.
    pub fn export_all_sources(&self) -> Result<HashMap<String, String>> {
        if let Some(store) = self.def_store() {
            let rows = Self::block_on(store.list("workflow"))?;
            return Ok(rows.into_iter().map(|r| (r.id, r.toml_source)).collect());
        }
        let mut map = HashMap::new();
        if !self.workflows_dir.exists() {
            return Ok(map);
        }
        for entry in std::fs::read_dir(&self.workflows_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "toml") {
                let content = std::fs::read_to_string(&path)?;
                // Parse to get the canonical ID from the TOML
                if let Ok(def) = WorkflowDefinition::from_toml(&content) {
                    map.insert(def.workflow.id, content);
                }
            }
        }
        Ok(map)
    }

    /// Validate and save a workflow definition (DB when a store is attached,
    /// else `workflows_dir`), then reload. `owner` is the member who owns it
    /// (`None` = system/operator); `visibility` says who else reaches it;
    /// `edit` records who/where in the revision history. Returns warnings for
    /// the author: the skills its `requires` names that this hub does not yet
    /// meet, which stop its runs until they are installed.
    pub async fn save_definition_source(
        &self,
        id: &str,
        toml_content: &str,
        owner: Option<&str>,
        visibility: Visibility,
        run_as: RunAsWrite,
        edit: workforce_orchestrator::workflow_store::EditContext<'_>,
    ) -> Result<Vec<String>> {
        // Validate the TOML parses correctly
        let def = WorkflowDefinition::from_toml(toml_content).wrap_err("Invalid workflow TOML")?;

        // Ensure the ID in the TOML matches the requested ID
        if def.workflow.id != id {
            return Err(eyre!(
                "Workflow ID mismatch: URL says '{}' but TOML contains '{}'",
                id,
                def.workflow.id
            ));
        }

        // Reject on save, never on load: a stored definition that predates this
        // check keeps running, but nothing new can introduce a reference that
        // fails only on the branch nobody exercised.
        let issues = def.authoring_issues();
        if !issues.is_empty() {
            return Err(eyre!(
                "Workflow '{}' has unresolvable template references:\n  - {}",
                id,
                issues.join("\n  - ")
            ));
        }

        if let Some(store) = self.def_store() {
            let parsed = serde_json::to_value(&def).ok();
            store
                .save(
                    id,
                    "workflow",
                    toml_content,
                    parsed,
                    owner,
                    visibility,
                    run_as,
                    edit,
                )
                .await?;
        } else if owner.is_some() {
            return Err(eyre!(
                "member-owned workflows require a database store; this deployment uses the filesystem"
            ));
        } else {
            if !workforce_common::config_files::is_plain_name(id) {
                return Err(eyre!("'{id}' is not a workflow id"));
            }
            std::fs::create_dir_all(&self.workflows_dir)?;
            let path = self.workflows_dir.join(format!("{}.toml", id));
            std::fs::write(&path, toml_content)
                .wrap_err_with(|| format!("Failed to write {}", path.display()))?;
        }

        // Reload immediately so a same-hub save is visible without waiting for
        // the DB poll (out-of-band edits are what polling is for).
        self.reload_definitions().await?;
        info!(id = %id, owner = owner.unwrap_or("system"),
            edited_by = edit.edited_by.unwrap_or(""), via = edit.edited_via,
            "Saved workflow definition");
        let installed = crate::wasm_skill_tools::global_skill_store()
            .map(|store| store.list_installed())
            .unwrap_or_default();
        let gated = approval_gated_tool_steps(&def, &installed);
        Ok(unmet_requirements(&def)
            .map(|unmet| {
                format!(
                    "saved, but its runs fail before their first step until this is met: {unmet}"
                )
            })
            .into_iter()
            .chain((!gated.is_empty()).then(|| {
                format!(
                    "saved, but these tool steps name a tool that requires human approval, \
                     which a workflow has nobody to ask for, so a run halts there: {}",
                    gated.join(", ")
                )
            }))
            .chain(
                def.decision_notices()
                    .into_iter()
                    .map(|notice| format!("saved, but {notice}")),
            )
            .collect())
    }

    /// Delete a workflow definition owned by `owner` (`None` = system) and
    /// reload. `edit` records who deleted it in the tombstone revision.
    pub async fn delete_definition(
        &self,
        id: &str,
        owner: Option<&str>,
        edit: workforce_orchestrator::workflow_store::EditContext<'_>,
    ) -> Result<()> {
        if let Some(store) = self.def_store() {
            store.delete(id, "workflow", owner, edit).await?;
        } else if owner.is_some() {
            return Err(eyre!(
                "member-owned workflows require a database store; this deployment uses the filesystem"
            ));
        } else {
            let path = self.workflows_dir.join(format!("{}.toml", id));
            if !path.exists() {
                return Err(eyre!("Workflow '{}' not found at {}", id, path.display()));
            }
            std::fs::remove_file(&path)
                .wrap_err_with(|| format!("Failed to delete {}", path.display()))?;
        }

        self.reload_definitions().await?;
        info!(id = %id, owner = owner.unwrap_or("system"),
            edited_by = edit.edited_by.unwrap_or(""), via = edit.edited_via,
            "Deleted workflow definition");
        Ok(())
    }

    /// Revision history (newest first, content omitted) for one definition.
    /// Requires the database store.
    pub async fn list_revisions(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> Result<Vec<workforce_orchestrator::workflow_store::DefRevision>> {
        let store = self
            .def_store()
            .ok_or_else(|| eyre!("revision history requires the database store"))?;
        store.list_revisions("workflow", id, owner).await
    }

    /// One revision with its full content. Requires the database store.
    pub async fn get_revision(
        &self,
        id: &str,
        owner: Option<&str>,
        rev: i32,
    ) -> Result<Option<workforce_orchestrator::workflow_store::DefRevision>> {
        let store = self
            .def_store()
            .ok_or_else(|| eyre!("revision history requires the database store"))?;
        store.get_revision("workflow", id, owner, rev).await
    }

    /// Whether `steerer` may `action` the run `instance_id`
    /// ([`crate::run_steering::decide`]), recorded either way. The run's
    /// workflow is read as its key resolves now; the terms it started under
    /// are read off the run.
    /// Whom a run's workflow reaches, as `reader` asks it.
    fn reach_of<'a>(&self, reader: &'a crate::def_write_auth::DefWriter) -> Reach<'a> {
        let team_store = self.service.team_store.read().clone();
        Reach {
            reader,
            teams: TeamReach::new(team_store.clone(), reader.identity_id.clone()),
            shares: crate::def_write_auth::StoredShares::new(self.def_store().cloned(), team_store),
        }
    }

    /// The runs of `rows` that `reader` may see: a run done for them or for
    /// nobody, any run to an administrator ([`run_is_for`]), and a run they
    /// may steer because its workflow reaches them and lets others steer its
    /// runs ([`crate::run_steering::decide`]), so whoever may steer a run sees
    /// it. A run whose workflow is gone keeps to the first rule. Each
    /// workflow's reach is looked up once for the whole batch.
    pub async fn readable_runs(
        &self,
        rows: Vec<WorkflowInstanceRow>,
        reader: &crate::def_write_auth::DefWriter,
    ) -> Vec<WorkflowInstanceRow> {
        let candidates: Vec<_> = {
            let definitions = self.definitions.read().await;
            rows.iter()
                .enumerate()
                .filter(|(_, row)| !run_is_for(row, reader))
                .filter_map(|(at, row)| {
                    let variables =
                        serde_json::from_str::<serde_json::Value>(&row.variables).ok()?;
                    let owned = definitions.get(&row.workflow_id)?;
                    let workflow = run_workflow(owned, &variables);
                    workflow
                        .terms
                        .by_others
                        .then(|| (at, principal_of(&variables), workflow, owned.owner.clone()))
                })
                .collect()
        };
        let reach = self.reach_of(reader);
        let decider = self.person(reader.identity_id.as_deref());
        let mut reached: HashMap<(&str, &'static str), bool> = HashMap::new();
        let mut steerable = HashSet::new();
        for (at, done_for, workflow, owner) in candidates {
            let key = (
                rows[at].workflow_id.as_str(),
                workflow.terms.visibility.as_str(),
            );
            let reaches = match reached.get(&key) {
                Some(reaches) => *reaches,
                None => *reached
                    .entry(key)
                    .or_insert(reach.reaches(&workflow, owner.as_deref()).await),
            };
            if crate::run_steering::decide(reader, &done_for, Some(&workflow), reaches).is_ok() {
                steerable.insert(at);
            }
        }
        rows.into_iter()
            .enumerate()
            .filter(|(at, row)| {
                steerable.contains(at)
                    || run_is_for(row, reader)
                    || Self::decides(row, decider.as_ref())
            })
            .map(|(_, row)| row)
            .collect()
    }

    /// The run `instance_id` when `reader` may see it ([`Self::readable_runs`]).
    pub async fn readable_run(
        &self,
        instance_id: &str,
        reader: &crate::def_write_auth::DefWriter,
    ) -> Result<Option<WorkflowInstanceRow>> {
        let Some(row) = self.get_instance_status(instance_id)? else {
            return Ok(None);
        };
        Ok(self.readable_runs(vec![row], reader).await.pop())
    }

    /// The definition at the composite `key` when `reader` may read and run
    /// it, by the rule the gateway's definition reads apply
    /// ([`crate::def_write_auth::may_read_def`]).
    pub async fn readable_definition(
        &self,
        key: &str,
        reader: &crate::def_write_auth::DefWriter,
    ) -> Option<WorkflowDefinition> {
        let owned = self.definitions.read().await.get(key).cloned()?;
        let reach = self.reach_of(reader);
        crate::def_write_auth::may_read_def(
            reader,
            &reach.shares,
            KIND_WORKFLOW,
            &owned.def.workflow.id,
            owned.owner.as_deref(),
            owned.visibility,
            &reach.teams,
        )
        .await
        .then_some(owned.def)
    }

    pub async fn authorize_steer(
        &self,
        instance_id: &str,
        steerer: &Steerer,
        action: SteerAction,
    ) -> std::result::Result<Steer, SteerRefused> {
        let unknown = |why: String| SteerRefused::Unknown(why);
        let row = self
            .telemetry_store
            .get_workflow_instance(instance_id)
            .map_err(|e| unknown(format!("{e:#}")))?
            .ok_or_else(|| unknown(format!("workflow instance '{instance_id}' not found")))?;
        // Only who the run acts as and the terms it started under are read:
        // a run whose history this build cannot read can still be stopped.
        // Variables that do not parse cannot say whose the run is, so only
        // an administrator may steer it.
        let Ok(variables) = serde_json::from_str::<serde_json::Value>(&row.variables) else {
            let verdict = match steerer.writer.is_admin {
                true => Ok(crate::run_steering::Standing::Admin),
                false => Err(unknown(format!(
                    "workflow instance '{instance_id}' does not say whose run it is"
                ))),
            };
            let nobody = TurnPrincipal::default();
            crate::run_steering::recorded(instance_id, &nobody, steerer, action, &verdict);
            return verdict.map(|standing| Steer::granted(steerer, standing));
        };
        let done_for = principal_of(&variables);
        let (workflow, owner) = match self.definitions.read().await.get(&row.workflow_id) {
            Some(owned) => (Some(run_workflow(owned, &variables)), owned.owner.clone()),
            None => (None, None),
        };
        let reaches = match &workflow {
            Some(workflow)
                if crate::run_steering::own_standing(&steerer.writer, &done_for).is_none() =>
            {
                self.reach_of(&steerer.writer)
                    .reaches(workflow, owner.as_deref())
                    .await
            }
            _ => false,
        };
        let verdict =
            crate::run_steering::decide(&steerer.writer, &done_for, workflow.as_ref(), reaches)
                .and_then(|standing| match instance_of(row) {
                    Ok(instance) => decision::decision_rule(&instance, action, standing),
                    Err(_) => Ok(standing),
                });
        crate::run_steering::recorded(instance_id, &done_for, steerer, action, &verdict);
        verdict.map(|standing| Steer::granted(steerer, standing))
    }

    /// Resume the paused (or failed) run `instance_id`, routing the step it
    /// stands at by `outcome` when that step has run. Whether the step went
    /// on with `outcome`, when one was given.
    pub async fn resume_workflow(
        self: &Arc<Self>,
        instance_id: &str,
        outcome: Option<String>,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
        by: &Steer,
    ) -> Result<Option<bool>> {
        let instance = self.load_instance(instance_id)?;
        let (channel_id, thread_id) = match by.redirects_delivery() {
            true => (channel_id, thread_id),
            false => delivery_context(&instance),
        };

        self.check_human_gate(&instance, &originating_agent_id)
            .await?;

        // A resume can arrive on any hub instance (HTTP API, tool, Slack);
        // refuse while an execute loop holds the lease elsewhere — resuming
        // would double-run it. Claiming here also pre-reserves the lease for
        // the loop this call spawns (idempotent for the same owner).
        if !self.telemetry_store.try_claim_workflow_instance(
            instance_id,
            process_instance_id(),
            WORKFLOW_LEASE_TTL_SECS,
        )? {
            return Err(eyre!(
                "workflow {instance_id} is actively executing on another hub instance — \
                 nothing to resume"
            ));
        }

        let def = self
            .definitions
            .read()
            .await
            .get(&instance.workflow_id)
            .map(|o| o.def.clone())
            .ok_or_else(|| eyre!("Workflow definition '{}' not found", instance.workflow_id))?;

        let (instance, intervention) = self.update_instance(instance_id, |instance| {
            if let Some(open) = run_decision::open_in(instance) {
                let starter =
                    crate::run_steering::starter(&instance_principal(instance)).map(str::to_string);
                return Err(eyre!("{}", decision::awaiting(&open, starter.as_deref())));
            }
            if instance.status == InstanceStatus::Failed {
                info!(instance_id = %instance_id, "Recovering failed workflow");
                recover_failed_to_paused(instance);
            }
            let intervention = instance.intervention(InterventionKind::Resume, by.actor().clone());
            // Whoever resumes the run by hand decides for it: a later pause of
            // theirs is not one reactivation lifts. Were its person still
            // suspended, the loop holds it again before its next step.
            take_suspension_hold(instance);
            // A settled decision held before it was carried out goes on to be
            // carried out; its step is not routed again.
            if run_decision::awaiting_follow(instance).is_some() {
                instance.status = InstanceStatus::Running;
                instance.updated_at = Utc::now();
                persist_delivery_context(
                    instance,
                    channel_id.as_deref(),
                    thread_id.as_deref(),
                    None,
                );
                return Ok((instance.clone(), intervention));
            }
            WorkflowExecutor::resume(instance, outcome.as_deref())?;

            // Re-run complete_step with the outcome to route via branches
            let should_complete = instance.current_step_id.clone().and_then(|step_id| {
                let response = instance.last_record_of(&step_id)?.agent_response.clone();
                Some((step_id, response))
            });
            let applied = match should_complete {
                Some((step_id, response)) => {
                    let transition = WorkflowExecutor::complete_step(&def, instance, &response)?;
                    went_on_with_resumed_outcome(instance, &step_id, &transition)
                }
                None => false,
            };
            persist_delivery_context(instance, channel_id.as_deref(), thread_id.as_deref(), None);
            let intervention = Intervention {
                applied: outcome.is_some().then_some(applied),
                outcome: outcome.clone(),
                ..intervention
            };
            Ok::<_, eyre::Report>((instance.clone(), intervention))
        })?;
        let applied = intervention.applied;
        crate::decision_records::moved(&instance, intervention);

        self.ensure_worktree_alive(instance_id).await;

        info!(instance_id = %instance_id, "Resumed workflow");

        let runtime = Arc::clone(self);
        let id = instance_id.to_string();
        tokio::spawn(async move {
            if let Err(e) = runtime
                .execute_loop(
                    &id,
                    channel_id.clone(),
                    thread_id.clone(),
                    originating_agent_id,
                )
                .await
            {
                let err_text = format!("{e}");
                error!(instance_id = %id, error = %err_text, "Workflow execution failed after resume");
                runtime
                    .post_terminal_status(
                        &id,
                        channel_id.as_deref(),
                        thread_id.as_deref(),
                        TerminalOutcome::Failed { error: &err_text },
                    )
                    .await;
            }
        });

        Ok(applied)
    }

    pub async fn abort_workflow(self: &Arc<Self>, instance_id: &str, by: &Steer) -> Result<()> {
        let instance = self.load_instance(instance_id)?;
        let (channel, thread) = delivery_context(&instance);
        let intervention = instance.intervention(InterventionKind::Abort, by.actor().clone());

        // Targeted status write, valid from ANY hub instance: never touches
        // step_history, so it cannot clobber a loop running elsewhere — that
        // loop notices the terminal status at its next step boundary.
        self.telemetry_store.cancel_workflow_instance(
            instance_id,
            "cancelled",
            "_cancellation_reason",
            "Cancelled by user",
        )?;
        crate::decision_records::ended(&instance.id, intervention);
        self.stop_decision(instance_id, self.actor_display(by.actor()))
            .await;

        // Cancel the running step immediately (agent checks token between LLM rounds)
        if let Some(token) = self.cancel_tokens.lock().await.remove(instance_id) {
            token.cancel();
            info!(instance_id = %instance_id, "Workflow aborted — cancelled running step");
        } else {
            info!(instance_id = %instance_id, "Workflow aborted");
        }

        self.wind_down_cancelled(
            instance_id,
            channel.as_deref(),
            thread.as_deref(),
            "Cancelled by user",
        )
        .await
    }

    /// Everything that follows a run ending cancelled: its terminal status
    /// posted, its last job and worktree ended, the runs it started stopped,
    /// and its slot offered to the queue.
    async fn wind_down_cancelled(
        self: &Arc<Self>,
        instance_id: &str,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        reason: &str,
    ) -> Result<()> {
        self.post_terminal_status(
            instance_id,
            channel_id,
            thread_id,
            TerminalOutcome::Cancelled { reason },
        )
        .await;

        // The run is cancelled, so no later job of its round starts: end the
        // one its step last reached.
        let instance = self.load_instance(instance_id)?;
        self.end_noted_job(&instance).await;
        self.cleanup_worktree(instance_id).await;
        self.cascade_abort_children(instance_id).await;
        self.promote_queued(&instance.workflow_id).await;
        Ok(())
    }

    /// Recursively abort all non-terminal child workflow instances of the given parent.
    async fn cascade_abort_children(self: &Arc<Self>, parent_id: &str) {
        let children = match self.telemetry_store.list_child_instances(parent_id) {
            Ok(c) => c,
            Err(e) => {
                warn!(parent_id = %parent_id, error = %e, "Failed to query child instances for cascade abort");
                return;
            }
        };

        for child in &children {
            info!(
                parent_id = %parent_id,
                child_id = %child.id,
                child_workflow = %child.workflow_id,
                child_status = %child.status,
                "Cascade-aborting child workflow instance"
            );

            // Cancel the child instance in the DB
            if let Err(e) = self.telemetry_store.update_workflow_instance_status(
                &child.id,
                "cancelled",
                child.current_step_id.as_deref(),
            ) {
                warn!(child_id = %child.id, error = %e, "Failed to cancel child instance");
                continue;
            }
            self.stop_decision(&child.id, Some("the run that started it".to_string()))
                .await;

            // Cancel the child's running step if active
            if let Some(token) = self.cancel_tokens.lock().await.remove(&child.id) {
                token.cancel();
            }

            // Clean up the child's worktree
            self.cleanup_worktree(&child.id).await;

            // Recurse for grandchildren
            Box::pin(self.cascade_abort_children(&child.id)).await;
        }

        if !children.is_empty() {
            info!(
                parent_id = %parent_id,
                count = children.len(),
                "Cascade abort completed for child instances"
            );
        }
        let workflow_ids: std::collections::BTreeSet<&str> =
            children.iter().map(|c| c.workflow_id.as_str()).collect();
        for workflow_id in workflow_ids {
            self.promote_queued(workflow_id).await;
        }
    }

    pub fn get_instance_status(&self, instance_id: &str) -> Result<Option<WorkflowInstanceRow>> {
        self.telemetry_store.get_workflow_instance(instance_id)
    }

    pub fn list_instances(
        &self,
        status: Option<&str>,
        workflow_id: Option<&str>,
    ) -> Result<Vec<WorkflowInstanceRow>> {
        self.telemetry_store
            .list_workflow_instances(status, workflow_id)
    }

    /// Point the Desk conversation of every live run `owner` started that
    /// `retarget` moves elsewhere: a running loop reads it again at its next
    /// step, a paused or queued run when it goes on. Returns the runs changed.
    pub fn retarget_desk(
        &self,
        owner: &str,
        retarget: &(dyn for<'t> Fn(&'t DeskTarget) -> Option<DeskTarget> + Send + Sync),
    ) -> Vec<String> {
        let live = ["running", "paused", "queued"]
            .into_iter()
            .filter_map(|status| match self.list_instances(Some(status), None) {
                Ok(rows) => Some(rows),
                Err(e) => {
                    warn!(status, error = %e, "Listing runs to point at another Desk conversation");
                    None
                }
            })
            .flatten();
        live.filter(|row| {
            serde_json::from_str::<serde_json::Value>(&row.variables)
                .ok()
                .and_then(|vars| serde_json::from_value::<DeskTarget>(vars.get(DESK_KEY)?.clone()).ok())
                .is_some_and(|target| retarget(&target).is_some())
        })
        .filter_map(|row| {
                let moved = self.update_instance(&row.id, |instance| {
                    let ours = instance_principal(instance).run_as.as_deref() == Some(owner);
                    let moved = desk_of(instance).filter(|_| ours).and_then(|t| retarget(&t));
                    persist_delivery_context(instance, None, None, moved.as_ref());
                    Ok::<_, eyre::Report>(moved.is_some())
                });
                match moved {
                    Ok(true) => Some(row.id),
                    Ok(false) => None,
                    Err(e) => {
                        warn!(instance_id = %row.id, error = %e, "A run could not be pointed elsewhere");
                        None
                    }
                }
            })
            .collect()
    }

    /// Every instance of this workflow that is still active (running, or
    /// parked at a pause gate). These count against the workflow's
    /// `max_concurrent`: a cron slot firing at the cap must not stack another
    /// instance — double-fires (overlapping deploys, stale per-agent jobs)
    /// otherwise post duplicate, contradictory reports — and a trigger start
    /// at the cap queues. A `queued` instance holds no slot, so it is not
    /// counted; otherwise the queue could never drain.
    pub fn active_instance_ids(&self, workflow_id: &str) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for status in ["running", "paused"] {
            ids.extend(
                self.list_instances(Some(status), Some(workflow_id))?
                    .into_iter()
                    .map(|row| row.id),
            );
        }
        Ok(ids)
    }

    /// The instance that stops a scheduled start of `workflow_id` right now:
    /// one of its active instances, when there are as many as the workflow's
    /// `max_concurrent` allows. `None` means a slot is free.
    pub async fn blocking_instance(&self, workflow_id: &str) -> Result<Option<String>> {
        let cap = self.max_concurrent(workflow_id).await;
        let active = self.active_instance_ids(workflow_id)?;
        Ok(blocking(&active, cap).cloned())
    }

    /// O(1) indexed lookup: find the workflow instance owning a Slack thread.
    pub fn find_instance_by_thread(
        &self,
        channel_id: &str,
        thread_id: &str,
        statuses: &[&str],
    ) -> Result<Option<WorkflowInstanceRow>> {
        self.telemetry_store
            .find_instance_by_thread(channel_id, thread_id, statuses)
    }

    // ─── Recovery & Maintenance ───

    /// Re-spawn `execute_loop` for every workflow instance that was `running` when the
    /// server last stopped, then promote from any queue a restart left holding
    /// a free slot. Called once during startup after surfaces are ready.
    pub async fn recover_running_instances(self: &Arc<Self>) {
        let instances = match self.list_instances(Some("running"), None) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "Failed to query running workflow instances for recovery");
                return;
            }
        };

        let mut recovered = 0u32;

        for row in &instances {
            // Local prefilter only — the authoritative cross-instance guard
            // is the execution lease `execute_loop` claims (a live loop on
            // another hub instance holds it, so the spawn below no-ops).
            if self.active_loops.lock().contains(&row.id) {
                continue;
            }

            let vars: serde_json::Value = serde_json::from_str(&row.variables).unwrap_or_default();
            let channel_id = vars
                .get("_channel_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            let thread_id = vars
                .get("_thread_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            let agent_id = row.agent_id.clone();

            let runtime = Arc::clone(self);
            let id = row.id.clone();
            tokio::spawn(async move {
                if let Err(e) = runtime
                    .execute_loop(&id, channel_id, thread_id, agent_id)
                    .await
                {
                    error!(instance_id = %id, error = %e, "Recovered workflow execution failed");
                }
            });
            recovered += 1;
        }

        if recovered > 0 {
            info!(
                count = recovered,
                "Recovered running workflow instances after restart"
            );
        }

        self.promote_all_queued().await;
    }

    /// Periodic maintenance loop: detects orphaned `running` instances that lost their
    /// `execute_loop` (e.g. due to a panic or timeout) and re-spawns them.
    /// Also cleans up stale worktrees.
    pub async fn start_maintenance_loop(
        self: &Arc<Self>,
        interval_secs: u64,
        drain_token: CancellationToken,
    ) {
        let every = |period: Duration| {
            let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticks
        };
        let mut maintenance = every(Duration::from_secs(interval_secs.max(1)));
        let mut decisions = every(DECISION_SWEEP_INTERVAL);
        info!(interval_secs, "Starting workflow maintenance loop");

        loop {
            tokio::select! {
                _ = drain_token.cancelled() => {
                    info!("Workflow maintenance loop stopping (drain)");
                    break;
                }
                _ = maintenance.tick() => {
                    self.run_maintenance_tick().await;
                }
                _ = decisions.tick() => {
                    self.decision_sweep().await;
                }
            }
        }
    }

    async fn run_maintenance_tick(self: &Arc<Self>) {
        // 1. Recover orphaned running instances
        let instances = match self.list_instances(Some("running"), None) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "Maintenance: failed to query running instances");
                return;
            }
        };

        let mut recovered = 0u32;

        for row in &instances {
            // Local prefilter; the execution lease is the real guard — a loop
            // live on any instance (including this one) holds it and the
            // spawned execute_loop below no-ops instead of duplicating.
            if self.active_loops.lock().contains(&row.id) {
                continue;
            }

            warn!(
                instance_id = %row.id,
                workflow_id = %row.workflow_id,
                "Maintenance: recovering possibly-orphaned workflow instance"
            );

            let vars: serde_json::Value = serde_json::from_str(&row.variables).unwrap_or_default();
            let channel_id = vars
                .get("_channel_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            let thread_id = vars
                .get("_thread_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            let agent_id = row.agent_id.clone();

            let runtime = Arc::clone(self);
            let id = row.id.clone();
            tokio::spawn(async move {
                if let Err(e) = runtime
                    .execute_loop(&id, channel_id.clone(), thread_id.clone(), agent_id)
                    .await
                {
                    let err_text = format!("{e}");
                    error!(instance_id = %id, error = %err_text, "Maintenance-recovered workflow failed");
                    runtime
                        .post_terminal_status(
                            &id,
                            channel_id.as_deref(),
                            thread_id.as_deref(),
                            TerminalOutcome::Failed { error: &err_text },
                        )
                        .await;
                }
            });
            recovered += 1;
        }

        if recovered > 0 {
            info!(
                count = recovered,
                "Maintenance: recovered orphaned workflow instances"
            );
        }

        // 1b. Drain queues whose promoting hub died between a run finishing
        // and the promotion landing — otherwise those instances wait forever.
        self.promote_all_queued().await;

        // 1c. Drop document-store rows past their TTL (reads already hide
        // them; this reclaims the space).
        if let Some(store) = crate::store::global_document_store() {
            match store.purge_expired().await {
                Ok(0) => {}
                Ok(n) => info!(count = n, "Maintenance: purged expired store documents"),
                Err(e) => warn!(error = %e, "Maintenance: store purge failed"),
            }
        }

        // 2. Clean up stale worktrees — pass active instance IDs to preserve their worktrees
        if let Some(mgr) = self.worktree_manager.get() {
            let all_instances = self.list_instances(None, None).unwrap_or_default();
            let active_ids: std::collections::HashSet<String> = all_instances
                .iter()
                .filter(|r| {
                    r.status != "completed"
                        && r.status != "failed"
                        && r.status != "exited"
                        && r.status != "cancelled"
                })
                .map(|r| r.id.clone())
                .collect();

            if let Err(e) = mgr
                .cleanup_stale(
                    &active_ids,
                    Duration::from_secs(86400),
                    Duration::from_secs(3600), // 1hr grace period for terminal workflows
                )
                .await
            {
                warn!(error = %e, "Maintenance: worktree cleanup failed");
            }
        }

        // 4. Restart runs held for a spent subscription window, and runs held
        // while the person they act as was suspended; re-arm the schedules of
        // anyone no longer suspended.
        self.resume_usage_capped().await;
        self.resume_suspension_held().await;
        self.rearm_held_schedules().await;
    }

    /// Resume every run held by `pause_on_usage_cap`, letting it re-run the
    /// step that was refused.
    ///
    /// There is no way to ask whether a subscription window has cleared
    /// without spending a call on it, so the probe *is* the retry: a still-spent
    /// window refuses in one turn for no tokens and the run parks again, while
    /// a cleared one simply carries on. The cost of guessing wrong is one
    /// instant refusal per held run per maintenance tick.
    async fn resume_usage_capped(self: &Arc<Self>) {
        self.resume_held(
            "a spent usage window",
            held_for_usage_cap,
            take_usage_cap_hold,
            park_for_usage_cap,
        )
        .await;
    }

    /// Resume every run held while the person it acts as was suspended,
    /// once whoever it acts as now is not. Runs on every maintenance tick,
    /// so a reactivation by any path, on any replica, releases them.
    pub(crate) async fn resume_suspension_held(self: &Arc<Self>) {
        self.resume_held(
            "a suspension",
            |vars| {
                held_for_suspension(vars)
                    && principal_of(vars)
                        .run_as
                        .is_none_or(|person| self.service.suspended(&person).is_none())
            },
            take_suspension_hold,
            repark_for_suspension,
        )
        .await;
    }

    /// Sync the schedule triggers again when anyone whose schedules the last
    /// sync held is no longer suspended.
    async fn rearm_held_schedules(&self) {
        let released = self
            .held_schedules
            .lock()
            .iter()
            .any(|person| self.service.suspended(person).is_none());
        if released {
            if let Err(e) = self.sync_schedule_triggers().await {
                warn!(error = %e, "Maintenance: re-arming released schedules failed");
            }
        }
    }

    /// Resume every paused run `ready` says its hold is over for: `take`
    /// drops the hold's markers and returns what `repark` needs to put them
    /// back when the resume does not happen.
    async fn resume_held(
        self: &Arc<Self>,
        hold: &str,
        ready: impl Fn(&serde_json::Value) -> bool,
        take: fn(&mut WorkflowInstance) -> String,
        repark: fn(&mut WorkflowInstance, &str),
    ) {
        let held = match self.list_instances(Some("paused"), None) {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "Maintenance: failed to query paused instances");
                return;
            }
        };

        for row in held {
            let vars: serde_json::Value = serde_json::from_str(&row.variables).unwrap_or_default();
            if !ready(&vars) {
                continue;
            }
            // A loop already live on this instance is mid-resume; leave it.
            if self.active_loops.lock().contains(&row.id) {
                continue;
            }

            let channel_id = vars
                .get("_channel_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            let thread_id = vars
                .get("_thread_id")
                .and_then(|v| v.as_str())
                .map(String::from);

            info!(
                instance_id = %row.id,
                workflow_id = %row.workflow_id,
                hold,
                "Maintenance: resuming a held run"
            );

            // Clear the markers *before* resuming, not after. Resuming spawns
            // the execution loop, so a step that is refused again re-parks the
            // instance while this call is still in flight — clearing afterwards
            // would wipe the hold that halt() had just written and strand the
            // run. Clearing first is safe in the other direction: a run that
            // gets going keeps no stale marker, which matters because a marker
            // left on a run that later pauses at a human gate would have this
            // sweep resume it straight past the person.
            let held_reason = match self
                .update_instance(&row.id, |instance| Ok::<_, eyre::Report>(take(instance)))
            {
                Ok(reason) => reason,
                Err(e) => {
                    warn!(instance_id = %row.id, error = %e, hold, "Maintenance: could not clear a hold");
                    continue;
                }
            };

            if let Err(e) = self
                .resume_workflow(&row.id, None, channel_id, thread_id, None, &Steer::hub())
                .await
            {
                // Another hub holding the lease is the ordinary case, not a
                // fault: that hub resumes it. Whatever the cause, the markers
                // have to go back on — without them this run is `paused` with
                // nothing left to notice it, and no later tick would ever
                // pick it up again.
                info!(instance_id = %row.id, error = %e, hold, "Maintenance: held run not resumed this tick");
                // Not paused any more: another hub got it going, which is
                // the outcome the hold was waiting for.
                if let Err(e) = self.update_instance(&row.id, |instance| {
                    if instance.status == InstanceStatus::Paused {
                        repark(instance, &held_reason);
                    }
                    Ok::<_, eyre::Report>(())
                }) {
                    error!(
                        instance_id = %row.id,
                        error = %e,
                        hold,
                        "Maintenance: a held run lost its hold and could not be re-marked — \
                         it will not resume by itself"
                    );
                }
            }
        }
    }

    // ─── Internal ───

    /// Entry point for every execute-loop caller. Re-establishes the instance's
    /// persisted `_run_as` identity (falling back to the caller's ambient scope)
    /// around the whole loop, so the capability gate and per-user vault/memory
    /// resolution see the executing identity on every path — interactive start,
    /// resume, startup recovery, and the maintenance sweep alike.
    async fn execute_loop(
        self: &Arc<Self>,
        instance_id: &str,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
    ) -> Result<()> {
        // Local half of the double-run guard: one loop per instance in this
        // process, registered for the WHOLE loop (the old per-agent-call
        // token left the instance unguarded between steps).
        if !self.active_loops.lock().insert(instance_id.to_string()) {
            info!(instance_id = %instance_id, "Skipping execute_loop — already active here");
            return Ok(());
        }
        // Cross-instance half: the DB execution lease. Losing the claim means
        // another hub instance is live-executing this workflow.
        let owner = process_instance_id();
        match self.telemetry_store.try_claim_workflow_instance(
            instance_id,
            owner,
            WORKFLOW_LEASE_TTL_SECS,
        ) {
            Ok(true) => {}
            Ok(false) => {
                self.active_loops.lock().remove(instance_id);
                info!(
                    instance_id = %instance_id,
                    "Skipping execute_loop — leased by another hub instance"
                );
                return Ok(());
            }
            Err(e) => {
                self.active_loops.lock().remove(instance_id);
                return Err(e);
            }
        }
        let renewal_stop = CancellationToken::new();
        {
            let stop = renewal_stop.clone();
            let store = Arc::clone(&self.telemetry_store);
            let id = instance_id.to_string();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = tokio::time::sleep(std::time::Duration::from_secs(
                            WORKFLOW_LEASE_RENEW_SECS,
                        )) => {}
                    }
                    match store.renew_workflow_lease(
                        &id,
                        process_instance_id(),
                        WORKFLOW_LEASE_TTL_SECS,
                    ) {
                        Ok(true) => {}
                        Ok(false) => {
                            warn!(
                                instance_id = %id,
                                "Lost workflow lease — another instance took over"
                            );
                            break;
                        }
                        Err(e) => {
                            warn!(instance_id = %id, error = %e, "Workflow lease renewal failed")
                        }
                    }
                }
            });
        }

        let principal = self
            .load_instance(instance_id)
            .ok()
            .map(|i| instance_principal(&i))
            .filter(|principal| *principal != TurnPrincipal::default())
            .unwrap_or_else(crate::user_config::current_principal);
        // Every step of one instance shares an artifact scope, so a table staged
        // by one step is readable by the next. Without it each step's own session
        // owns its artifacts and the handoff can only ever fail. The loop is
        // boxed: its future is tens of kilobytes, and unboxed it is copied
        // whole through every layer that spawns or wraps this one.
        let desk = self
            .load_instance(instance_id)
            .ok()
            .as_ref()
            .and_then(desk_of)
            .zip(principal.run_as.clone())
            .map(|(target, owner)| DeskScope {
                target,
                owner,
                attended: false,
                relayed: false,
            });
        let result = self
            .run_scoped(
                principal,
                workforce_agent::runner::with_desk(
                    desk,
                    workforce_agent::runner::CURRENT_ARTIFACT_SCOPE.scope(
                        artifact_scope(instance_id),
                        Box::pin(self.execute_loop_inner(
                            instance_id,
                            channel_id,
                            thread_id,
                            originating_agent_id,
                        )),
                    ),
                ),
            )
            .await;

        renewal_stop.cancel();
        self.active_loops.lock().remove(instance_id);
        if let Err(e) = self
            .telemetry_store
            .release_workflow_instance(instance_id, owner)
        {
            warn!(instance_id = %instance_id, error = %e, "Failed to release workflow lease");
        }
        match result {
            Err(e) if e.to_string().contains(FENCED) => {
                warn!(
                    instance_id = %instance_id,
                    "Execute loop fenced out — another instance owns this workflow now"
                );
                Ok(())
            }
            Err(e) if format!("{e:#}").contains(UNREADABLE) => {
                warn!(
                    instance_id = %instance_id,
                    error = %format!("{e:#}"),
                    "Execute loop stopped — a newer build wrote this run's history; it is left for one"
                );
                Ok(())
            }
            other => {
                self.promote_queued_after(instance_id).await;
                if other.is_ok() && !shutdown_cancelled(&self.shutdown) {
                    self.hand_on(instance_id);
                }
                other
            }
        }
    }

    /// Start a fresh loop for a run this loop left `running`: a change that
    /// landed outside the loop as it exited (a settled decision) found it
    /// still live and spawned nothing.
    fn hand_on(self: &Arc<Self>, instance_id: &str) {
        let Some(instance) = self
            .load_instance(instance_id)
            .ok()
            .filter(|instance| instance.status == InstanceStatus::Running)
        else {
            return;
        };
        info!(instance_id = %instance_id, "Run left running as its loop exited — handing it to a fresh loop");
        let (channel_id, thread_id) = delivery_context(&instance);
        self.spawn_execute_loop(
            instance.id.clone(),
            channel_id,
            thread_id,
            instance.agent_id.clone(),
            instance_principal(&instance),
        );
    }

    /// The slot `instance_id` held is free once its persisted status is
    /// terminal — a pause keeps it — so offer it to the workflow's queue.
    async fn promote_queued_after(self: &Arc<Self>, instance_id: &str) {
        if let Some(workflow_id) = self
            .load_instance(instance_id)
            .ok()
            .filter(WorkflowInstance::is_terminal)
            .map(|instance| instance.workflow_id)
        {
            self.promote_queued(&workflow_id).await;
        }
    }

    async fn execute_loop_inner(
        self: &Arc<Self>,
        instance_id: &str,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
    ) -> Result<()> {
        let fallback = originating_agent_id.as_deref();

        let mut thread_id = thread_id;

        {
            let instance = self.load_instance(instance_id)?;
            if instance.step_history.is_empty() {
                if let Some(def) = self
                    .definitions
                    .read()
                    .await
                    .get(&instance.workflow_id)
                    .map(|o| o.def.clone())
                {
                    // Reports delivery posts no banner: the run's only channel
                    // messages are the reports its steps declare (and any
                    // pause/failure card, which anchors the thread instead).
                    if def.workflow.delivery == DeliveryMode::Full {
                        let plan_msg_id = self
                            .announce_workflow_plan(
                                &def,
                                channel_id.as_deref(),
                                thread_id.as_deref(),
                                None,
                                fallback,
                            )
                            .await;

                        // If we didn't have a thread_id, anchor to the first message we posted.
                        // This ensures all subsequent messages and command matching use the
                        // correct thread, fixing the approve/reject/abort routing.
                        if thread_id.is_none() {
                            if let Some(ref msg_id) = plan_msg_id {
                                info!(instance_id = %instance_id, thread_id = %msg_id, "Anchoring workflow to Slack thread");
                                thread_id = Some(msg_id.clone());
                                self.anchor_thread(instance_id, channel_id.as_deref(), msg_id);
                            }
                        }
                    }
                }
            }
        }

        let mut emitted_steps = 0usize;
        loop {
            if shutdown_cancelled(&self.shutdown) {
                info!(
                    instance_id = %instance_id,
                    "Shutting down — leaving workflow for recovery to resume"
                );
                break;
            }
            let mut instance = self.load_instance(instance_id)?;
            self.emit_step_completions(instance_id, &instance, &mut emitted_steps)
                .await;

            if instance.is_terminal() {
                break;
            }
            if instance.status == InstanceStatus::Paused {
                info!(instance_id = %instance_id, "Workflow is paused — stopping execution loop");
                break;
            }
            // Only promotion runs a queued instance, and it sets `running`
            // first; a loop that finds the queue status is not its turn.
            if instance.status == InstanceStatus::Queued {
                info!(instance_id = %instance_id, "Workflow is queued — stopping execution loop");
                break;
            }
            if run_decision::cards_to_close(&instance).is_some() {
                self.close_cards(instance_id).await;
                instance = self.load_instance(instance_id)?;
            }
            if self.held_person(&instance).is_some() {
                park_for_suspension(&mut instance);
                self.save_instance(&instance)?;
                info!(instance_id = %instance_id, "The person this run acts as is suspended — run held before its next step");
                break;
            }

            let owned = self
                .definitions
                .read()
                .await
                .get(&instance.workflow_id)
                .cloned()
                .ok_or_else(|| eyre!("Workflow definition '{}' not found", instance.workflow_id));
            // Persist the failure before propagating: leaving the instance in
            // "running" makes the maintenance sweep re-run it forever and keeps
            // its Slack thread locked to workflow commands.
            let owned = match owned {
                Ok(o) => o,
                Err(e) => {
                    WorkflowExecutor::fail(&mut instance, &e.to_string());
                    self.save_instance(&instance)?;
                    return Err(e);
                }
            };
            let def = owned.def.clone();
            let delivery = def.workflow.delivery;
            // Progress chrome (step/phase announcements, live status, retry
            // notices) belongs to Full delivery. Reports delivery keeps the
            // channel to declared reports plus pause/failure cards.
            let announce = delivery == DeliveryMode::Full;
            if let Some(settled) = run_decision::awaiting_follow(&instance) {
                match self
                    .follow_decision(&def, &mut instance, settled, announce)
                    .await?
                {
                    decision::Followed::Continue => continue,
                    decision::Followed::Stop => break,
                }
            }
            let secret_policy = self.build_secret_policy(&owned);

            if instance.step_history.is_empty() {
                if let Some(unmet) = unmet_requirements(&def) {
                    self.halt(
                        &mut instance,
                        &unmet,
                        &unmet,
                        channel_id.as_deref(),
                        thread_id.as_deref(),
                        def.workflow.agent.as_deref(),
                        fallback,
                    )
                    .await?;
                    break;
                }
            }

            let mut exec = match WorkflowExecutor::prepare_step(&def, &instance) {
                Ok(exec) => exec,
                Err(e) => {
                    WorkflowExecutor::fail(&mut instance, &e.to_string());
                    self.save_instance(&instance)?;
                    return Err(e);
                }
            };
            self.emit_stream_event(
                instance_id,
                WorkflowStreamEvent::StepStarted {
                    step_id: exec.step_id.clone(),
                },
            )
            .await;

            let step_index = def.step_index(&exec.step_id).unwrap_or(0) + 1;
            let total = def.steps.len();
            let step_name = def
                .get_step(&exec.step_id)
                .map(|s| s.name.as_str())
                .unwrap_or(&exec.step_id);

            let (step_service, step_agent_id) =
                match self.resolve_step_service(exec.agent_id.as_deref()) {
                    Ok(resolved) => resolved,
                    Err(reason) => {
                        let msg = format!("Step '{}': {}", exec.step_id, reason);
                        WorkflowExecutor::fail(&mut instance, &msg);
                        self.save_instance(&instance)?;
                        return Err(eyre!(msg));
                    }
                };
            info!(
                instance_id = %instance_id,
                step = %exec.step_id,
                agent = %step_agent_id,
                "Routing step to agent"
            );

            let step_msg_id = if announce {
                self.send_status(
                    &format!("Step {}/{} **{}**", step_index, total, step_name),
                    channel_id.as_deref(),
                    thread_id.as_deref(),
                    Some(step_agent_id.as_str()),
                    fallback,
                )
                .await
            } else {
                None
            };

            // Second-chance thread anchoring: if the plan announcement didn't
            // establish a thread (e.g. surface was unavailable), anchor to the
            // step announcement message instead.
            if thread_id.is_none() {
                if let Some(ref msg_id) = step_msg_id {
                    info!(instance_id = %instance_id, thread_id = %msg_id, "Anchoring workflow to Slack thread");
                    thread_id = Some(msg_id.clone());
                    self.anchor_thread(instance_id, channel_id.as_deref(), msg_id);
                    anchored(&mut instance, channel_id.as_deref(), msg_id);
                }
            }

            // Each step runs in an isolated session keyed by instance + step
            let channel = format!("workflow:{}:{}", instance.id, exec.step_id);

            let ticket_id = booked_ticket(&instance.variables).map(str::to_string);

            let started_at = Utc::now();

            let resolved_model = exec.model.as_ref().map(|m| {
                let current_tier_key = step_service.tier_key.read().clone();
                let tiers = step_service.tier_registry();
                let (routing_key, model) = tiers.resolve_routed_model(m, &current_tier_key);
                TierRegistry::format_routed(routing_key.as_deref(), model)
            });

            // Pre-load skills declared by this workflow step so tools are
            // available regardless of which model runs the step.
            for skill_name in &exec.skills {
                if crate::skill_loader::skill_tools_registered(skill_name, &step_service.tools) {
                    continue;
                }
                let Some(ref vault) = step_service.vault else {
                    continue;
                };
                match crate::skill_loader::load_and_track(
                    skill_name,
                    Arc::clone(vault),
                    step_service.global_vault.clone(),
                    &step_service.config,
                    &step_service.tools,
                    &step_service.skill_state,
                    "workflow-step",
                ) {
                    Ok(result) => info!(
                        instance_id = %instance_id,
                        skill = %skill_name,
                        tools = ?result.tool_names,
                        "Pre-loaded skill for workflow step"
                    ),
                    Err(e) => warn!(
                        instance_id = %instance_id,
                        skill = %skill_name,
                        error = %e,
                        "Failed to pre-load skill for workflow step"
                    ),
                }
            }

            // ── Step execution: branch on step mode, then engine type ──
            // What served the step and what it cost, stamped on its record.
            let mut provenance = StepProvenance {
                definition_hash: owned.content_hash.clone(),
                ..Default::default()
            };
            let mut step_cost_usd = 0.0_f64;
            // A step that declares `decide_first` is put to the decision
            // model before its turn. An answer that settles it stands in for
            // the turn; nothing is saved until the step is recorded and
            // routed below, so an interrupted loop simply asks again.
            let first = self
                .ask_first(&instance, def.get_step(&exec.step_id), &exec, &step_service)
                .await;
            let delegated = self
                .run_delegated_step(
                    &mut instance,
                    &def,
                    &exec,
                    DelegatedContext {
                        step_service: &step_service,
                        channel_id: channel_id.as_deref(),
                        thread_id: thread_id.as_deref(),
                        step_agent_id: &step_agent_id,
                    },
                    &mut provenance,
                )
                .await;
            let stand_in = first
                .as_ref()
                .and_then(|first| first.stand_in(&mut exec.mode, &mut provenance));
            let (response, actual_rounds, budget_capped) = if let Some(response) = stand_in {
                (response, 0, false)
            } else if let Some(dispatched) = delegated {
                match dispatched {
                    Ok(Some(response)) => {
                        provenance.engine = exec.mode.provenance_engine().map(Into::into);
                        // Persist the answer before anything else can
                        // interrupt: from here to the step record the work
                        // must never run again.
                        note_reported_tool_result(&mut instance, &exec.step_id, &response);
                        self.save_instance(&instance)?;
                        (response, 0, false)
                    }
                    Ok(None) => {
                        info!(
                            instance_id = %instance_id,
                            step = %exec.step_id,
                            "Workflow was stopped while its delegated step was in flight"
                        );
                        break;
                    }
                    Err(msg) => {
                        let msg = format!("Step '{}' failed: {}", exec.step_id, msg);
                        error!(instance_id = %instance_id, step = %exec.step_id, %msg);
                        self.halt(
                            &mut instance,
                            &msg,
                            &msg,
                            channel_id.as_deref(),
                            thread_id.as_deref(),
                            Some(step_agent_id.as_str()),
                            fallback,
                        )
                        .await?;
                        break;
                    }
                }
            } else if exec.mode == StepMode::Report {
                provenance.engine = Some(REPORT_ENGINE.into());
                (String::new(), 0, false)
            } else if is_coding_engine(&exec.engine) {
                if exec.persona.is_some() {
                    tracing::debug!(
                        step = %exec.step_id,
                        engine = %exec.engine,
                        "persona override set but ignored for a coding engine (default engine only)"
                    );
                }
                // A coding engine delegates the entire step to the CLI in its
                // sandbox. Errors NEVER fall through to the normal agent loop.
                let engine = match self.coding_engine_for(&exec) {
                    Ok(engine) => engine,
                    Err(e) => {
                        let msg = format!("Coding engine chain cannot be built: {e}");
                        error!(instance_id = %instance_id, step = %exec.step_id, %msg);
                        self.halt(
                            &mut instance,
                            &msg,
                            &msg,
                            channel_id.as_deref(),
                            thread_id.as_deref(),
                            Some(step_agent_id.as_str()),
                            fallback,
                        )
                        .await?;
                        break;
                    }
                };
                let cc_identity_id = crate::user_config::current_identity_id();
                let cc_description = format!("workflow-cc:{}", exec.step_id);
                let ctx = RoundContext {
                    service: &step_service,
                    identity: cc_identity_id.as_deref(),
                    stand_in: WORKFLOW_ENGINE,
                    ticket_id: ticket_id.as_deref(),
                    description: &cc_description,
                    resume: crate::mcp_gate::RouteResume::Nowhere,
                };
                if let Err(reason) = engine.preflight(&ctx) {
                    let msg = format!("Claude Code engine cannot run this step: {reason}");
                    error!(instance_id = %instance_id, step = %exec.step_id, %msg);
                    self.halt(
                        &mut instance,
                        &msg,
                        &msg,
                        channel_id.as_deref(),
                        thread_id.as_deref(),
                        Some(step_agent_id.as_str()),
                        fallback,
                    )
                    .await?;
                    break;
                }

                // Where the step runs: a scope on the coding worker when the
                // install's executor is remote, else this host's worktree.
                let sandbox = match self.remote_launcher.get() {
                    Some(launcher) => {
                        self.remote_step_sandbox(launcher, &instance, &step_service)
                            .await
                    }
                    None => {
                        let workdir =
                            Self::resolve_step_workdir(&instance, &step_service.workspace);
                        Ok(crate::coding_sandbox::adopt_workdir(
                            workdir,
                            self.step_hub_tools(),
                            workforce_agent::tools::exec_runner::RunnerEndpoint::from_config(
                                &step_service.config.exec,
                            ),
                        ))
                    }
                };
                let sandbox = match sandbox {
                    Ok(sandbox) => sandbox,
                    Err(e) => {
                        let msg = format!(
                            "Claude Code step could not get a workspace on the coding worker: {e}"
                        );
                        error!(instance_id = %instance_id, step = %exec.step_id, %msg);
                        self.halt(
                            &mut instance,
                            &msg,
                            &msg,
                            channel_id.as_deref(),
                            thread_id.as_deref(),
                            Some(step_agent_id.as_str()),
                            fallback,
                        )
                        .await?;
                        break;
                    }
                };

                // On a worker that keeps its jobs through a restart, the round
                // is noted on the instance before it starts, so the hub that
                // picks the run up collects it rather than running it again.
                let kept = sandbox.placement().is_some_and(|placement| placement.jobs);
                let journal = match coding_round::step_round(&instance, &exec.step_id, kept) {
                    coding_round::StepRound::Unkeyed => None,
                    coding_round::StepRound::Keyed(note, jobs) => {
                        coding_round::keep_note(&mut instance, &note);
                        self.save_instance(&instance)?;
                        Some((
                            Arc::new(coding_round::StepJournal::new(
                                Arc::clone(self),
                                instance_id,
                                note,
                            )),
                            jobs,
                        ))
                    }
                    coding_round::StepRound::Unknown => {
                        let msg = format!(
                            "{} step '{}' was interrupted and its round cannot be collected, so its outcome is unknown. Check the worktree before retrying.",
                            engine.label(),
                            exec.step_id
                        );
                        error!(instance_id = %instance_id, step = %exec.step_id, %msg);
                        self.halt(
                            &mut instance,
                            &msg,
                            &msg,
                            channel_id.as_deref(),
                            thread_id.as_deref(),
                            Some(step_agent_id.as_str()),
                            fallback,
                        )
                        .await?;
                        break;
                    }
                };

                let round = coding_engine::CodingRound {
                    jobs: journal.as_ref().map(|(_, jobs)| jobs.clone()),
                    max_turns: exec.max_rounds,
                    model: exec.model.clone(),
                    effort: exec.effort.clone(),
                    tools: step_allowed_tools(&exec),
                    // The token the step's repo was fetched with; a remote
                    // scope has no credential helpers of its own.
                    gh_token: Self::target_repo(&instance)
                        .and_then(|repo| repo.split_once('/'))
                        .and_then(|(owner, _)| {
                            crate::user_config::resolve_github_pat(
                                step_service.vault.as_deref(),
                                step_service.global_vault.as_deref(),
                                owner,
                            )
                        }),
                    ..coding_engine::CodingRound::one_shot(
                        exec.task_prompt.clone(),
                        std::time::Duration::from_secs(
                            exec.timeout_secs.unwrap_or(DEFAULT_STEP_TIMEOUT_SECS),
                        ),
                    )
                };

                let cancel_token = CancellationToken::new();
                self.cancel_tokens
                    .lock()
                    .await
                    .insert(instance_id.to_string(), cancel_token.clone());

                if announce {
                    self.send_status(
                        "Delegating step to Claude Code CLI...",
                        channel_id.as_deref(),
                        thread_id.as_deref(),
                        Some(step_agent_id.as_str()),
                        fallback,
                    )
                    .await;
                }

                info!(
                    instance_id = %instance_id,
                    step = %exec.step_id,
                    workdir = %sandbox.workdir().display(),
                    max_turns = ?round.max_turns,
                    engine = engine.label(),
                    "Executing step via Claude Code CLI"
                );

                let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(64);
                let progress_surface = self
                    .get_surface_for_agent(Some(step_agent_id.as_str()), fallback)
                    .await;
                let progress_channel_id = if announce { channel_id.clone() } else { None };
                let progress_thread_id = thread_id.clone();
                let progress_step_msg_id = step_msg_id.clone();
                tokio::spawn(async move {
                    consume_step_progress(
                        progress_rx,
                        progress_surface,
                        progress_channel_id,
                        progress_thread_id,
                        progress_step_msg_id,
                    )
                    .await;
                });

                let run = engine.run(
                    Arc::clone(&sandbox),
                    &round,
                    &ctx,
                    Some(cancel_token),
                    Some(progress_tx),
                );
                let cc_result = match &journal {
                    Some((journal, _)) => {
                        let noted: Arc<dyn workforce_agent::turn_journal::TurnJournal> =
                            journal.clone();
                        let result = workforce_agent::turn_journal::ToolCall::new(
                            Some(noted),
                            serde_json::Value::Null,
                        )
                        .scope(run)
                        .await;
                        coding_round::keep_note(&mut instance, &journal.note());
                        result
                    }
                    None => run.await,
                };

                self.cancel_tokens.lock().await.remove(instance_id);

                // A round ended because the run was stopped, paused or moved
                // on is not the step's failure.
                let failed = !matches!(&cc_result, Ok(r) if r.exit_code == 0);
                if failed && self.stopped_during_step(instance_id, &exec.step_id)? {
                    break;
                }

                let (booked, cost) =
                    round_booking(std::mem::take(&mut provenance), engine.label(), &cc_result);
                provenance = booked;
                step_cost_usd = cost;

                match cc_result {
                    Ok(r) if r.exit_code == 0 => {
                        info!(
                            instance_id = %instance_id,
                            step = %exec.step_id,
                            duration_secs = r.duration.as_secs(),
                            turns = r.num_turns,
                            input_tokens = r.input_tokens,
                            output_tokens = r.output_tokens,
                            cost_usd = format!("{:.4}", r.cost_usd),
                            "Claude Code step completed"
                        );
                        (r.text, r.num_turns, false)
                    }
                    other => {
                        let rounds = other.as_ref().map_or(0, |r| r.num_turns);
                        let msg = match other {
                            Ok(r) => format!(
                                "{} failed (exit {}). Halted to prevent API cost fallback.\n{}",
                                engine.label(),
                                r.exit_code,
                                narration_tail(&r.text)
                            ),
                            Err(e) => format!(
                                "{} error: {e}. Halted to prevent API cost fallback.",
                                engine.label()
                            ),
                        };
                        error!(instance_id = %instance_id, step = %exec.step_id, %msg);
                        instance.record_failed_attempt(
                            &exec.step_id,
                            &msg,
                            started_at,
                            rounds,
                            step_cost_usd,
                            provenance.clone(),
                        );

                        add_step_reaction(
                            self.get_surface_for_agent(Some(step_agent_id.as_str()), fallback)
                                .await,
                            channel_id.as_deref(),
                            step_msg_id.as_deref(),
                            channels::indicator::ERROR,
                        )
                        .await;

                        self.halt(
                            &mut instance,
                            &msg,
                            &msg,
                            channel_id.as_deref(),
                            thread_id.as_deref(),
                            Some(step_agent_id.as_str()),
                            fallback,
                        )
                        .await?;
                        break;
                    }
                }
            } else if exec.engine == crate::claude_pilot::ENGINE {
                if exec.persona.is_some() {
                    tracing::debug!(
                        step = %exec.step_id,
                        "persona override set but ignored for claude-pilot engine (default engine only)"
                    );
                }
                // ── Claude Pilot engine: orchestrated multi-phase execution ────
                let workdir = Self::resolve_step_workdir(&instance, &step_service.workspace);

                let vault_ref = step_service.vault.as_deref();
                let global_vault_ref = step_service.global_vault.as_deref();
                let login = match step_service
                    .logins
                    .resolve(workforce_common::subscription::SubscriptionKind::Claude)
                {
                    Ok(login) => login,
                    Err(no_login) => {
                        let msg = crate::agent_turn::connect::no_login_notice(
                            step_agent_id.as_str(),
                            &no_login,
                        );
                        error!(instance_id = %instance_id, step = %exec.step_id, %msg, "Claude Pilot has no login to run on");
                        WorkflowExecutor::fail(&mut instance, &msg);
                        self.save_instance(&instance)?;
                        break;
                    }
                };

                if exec.phases.is_empty() {
                    let msg = "Claude Pilot engine requires at least one phase defined in [[step.phases]]";
                    error!(instance_id = %instance_id, step = %exec.step_id, msg);
                    WorkflowExecutor::fail(&mut instance, msg);
                    self.save_instance(&instance)?;
                    break;
                }

                let phases: Vec<crate::claude_pilot::PilotPhase> = phase_tasks(&exec)
                    .zip(&exec.phases)
                    .map(|(task, p)| crate::claude_pilot::PilotPhase {
                        name: p.name.clone(),
                        task,
                        allowed_tools: p.allowed_tools.clone(),
                        max_turns: p.max_turns,
                        mcp: p.mcp,
                    })
                    .collect();

                let cancel_token = CancellationToken::new();
                let principal = crate::user_config::current_identity_id();
                let patterns = self.step_hub_tools();
                let mcp = (!patterns.is_empty())
                    .then(|| {
                        crate::mcp_turn::RoundTools::bind(
                            &step_service,
                            principal.as_deref().unwrap_or(WORKFLOW_ENGINE),
                            crate::mcp_turn::McpExposure::Patterns(patterns.into()),
                            Some(&cancel_token),
                            crate::mcp_turn::ClientTimeout::CLAUDE_CODE,
                        )
                    })
                    .flatten()
                    .map(|tools| {
                        tools.unattended().with_model(
                            exec.model
                                .clone()
                                .unwrap_or_else(|| crate::claude_pilot::ENGINE.to_string()),
                        )
                    });

                let pilot_config = crate::claude_pilot::PilotConfig {
                    phases,
                    workdir,
                    model: exec.model.clone(),
                    effort: exec.effort.clone(),
                    mcp,
                    timeout: std::time::Duration::from_secs(30 * 60),
                    agent_id: step_agent_id.to_string(),
                };

                self.cancel_tokens
                    .lock()
                    .await
                    .insert(instance_id.to_string(), cancel_token.clone());

                if announce {
                    self.send_status(
                        "Executing step via Claude Pilot (multi-phase)...",
                        channel_id.as_deref(),
                        thread_id.as_deref(),
                        Some(step_agent_id.as_str()),
                        fallback,
                    )
                    .await;
                }

                info!(
                    instance_id = %instance_id,
                    step = %exec.step_id,
                    phases = exec.phases.len(),
                    "Executing step via Claude Pilot"
                );

                let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(64);
                let progress_surface = self
                    .get_surface_for_agent(Some(step_agent_id.as_str()), fallback)
                    .await;
                let progress_channel_id = if announce { channel_id.clone() } else { None };
                let progress_thread_id = thread_id.clone();
                let progress_step_msg_id = step_msg_id.clone();
                tokio::spawn(async move {
                    consume_step_progress(
                        progress_rx,
                        progress_surface,
                        progress_channel_id,
                        progress_thread_id,
                        progress_step_msg_id,
                    )
                    .await;
                });

                let _pilot_started_at = chrono::Utc::now();
                let pilot_result = crate::claude_pilot::run_pilot(
                    &pilot_config,
                    &login,
                    vault_ref,
                    global_vault_ref,
                    Some(cancel_token),
                    Some(progress_tx),
                )
                .await;

                self.cancel_tokens.lock().await.remove(instance_id);

                let pilot_model = coding_engine::labelled_model(
                    crate::claude_pilot::ENGINE,
                    exec.model.as_deref(),
                );
                let pilot_description = format!("workflow-pilot:{}", exec.step_id);

                provenance.engine = Some(crate::claude_pilot::ENGINE.into());
                if let Ok(r) = &pilot_result {
                    let pilot_identity_id = crate::user_config::current_identity_id();
                    step_service
                        .cost_tracker
                        .record(workforce_common::cost::CostEntry {
                            task_type: Some("coding"),
                            ticket_id: ticket_id.as_deref(),
                            identity_id: pilot_identity_id.as_deref(),
                            description: Some(&pilot_description),
                            served: Some(login.served(
                                workforce_common::subscription::SubscriptionRoute::ClaudeCode,
                                step_agent_id.as_str(),
                            )),
                            ..workforce_common::cost::CostEntry::new(
                                step_agent_id.as_str(),
                                &pilot_model,
                                workforce_common::cost::TokenCounts {
                                    input: r.input_tokens,
                                    output: r.output_tokens,
                                    ..Default::default()
                                },
                                workforce_common::subscription::Origin::Workflow,
                            )
                        });
                    provenance.model = exec.model.clone();
                    provenance.input_tokens = Some(r.input_tokens);
                    provenance.output_tokens = Some(r.output_tokens);
                    step_cost_usd = r.cost_usd;
                }

                match pilot_result {
                    Ok(r) if r.exit_code == 0 => {
                        info!(
                            instance_id = %instance_id,
                            step = %exec.step_id,
                            duration_secs = r.duration.as_secs(),
                            turns = r.num_turns,
                            input_tokens = r.input_tokens,
                            output_tokens = r.output_tokens,
                            cost_usd = format!("{:.4}", r.cost_usd),
                            phases_completed = r.phases_completed,
                            phases_total = r.phases_total,
                            "Claude Pilot step completed"
                        );
                        (r.text, r.num_turns, false)
                    }
                    other => {
                        let pilot_step_def = def.get_step(&exec.step_id);
                        let (msg, rounds) = match other {
                            Ok(r) => (
                                format_pilot_failure_message(
                                    &exec.step_id,
                                    pilot_step_def,
                                    &r,
                                    instance_id,
                                ),
                                r.num_turns,
                            ),
                            Err(e) => (
                                format!(
                                    ":x: *Workflow failed* at step `{}`\n\
                                     • *Error:* {e}\n\
                                     • *Instance:* `{instance_id}`\n\n\
                                     _This was an infrastructure error (network, auth, or subprocess spawn), \
                                     not a problem with the code itself._",
                                    exec.step_id
                                ),
                                0,
                            ),
                        };
                        error!(instance_id = %instance_id, step = %exec.step_id, %msg);
                        instance.record_failed_attempt(
                            &exec.step_id,
                            &msg,
                            started_at,
                            rounds,
                            step_cost_usd,
                            provenance.clone(),
                        );

                        add_step_reaction(
                            self.get_surface_for_agent(Some(step_agent_id.as_str()), fallback)
                                .await,
                            channel_id.as_deref(),
                            step_msg_id.as_deref(),
                            channels::indicator::ERROR,
                        )
                        .await;

                        self.halt(
                            &mut instance,
                            &msg,
                            &msg,
                            channel_id.as_deref(),
                            thread_id.as_deref(),
                            Some(step_agent_id.as_str()),
                            fallback,
                        )
                        .await?;
                        break;
                    }
                }
            } else {
                // Default engine: single-shot or, when `[[step.phases]]` is
                // declared, N calls against the same session so messages
                // accumulate across phases (in-process equivalent of
                // claude-pilot's `--resume`).

                let base_whitelist: Option<Vec<String>> = if exec.exclusive_tools {
                    let mut whitelist = exec.preferred_tools.clone();
                    for skill_name in &exec.skills {
                        let prefix = format!("{}.", skill_name);
                        for name in step_service.tools.names() {
                            if name.starts_with(&prefix) {
                                whitelist.push(name);
                            }
                        }
                    }
                    Some(whitelist)
                } else {
                    None
                };

                let phase_calls = build_phase_calls(&exec, base_whitelist);

                let total_phases = phase_calls.len();
                let mut response = String::new();
                let mut total_rounds = 0u32;
                let mut any_budget_capped = false;
                let mut failed = false;

                'phase_loop: for (i, call) in phase_calls.iter().enumerate() {
                    if announce && !call.name.is_empty() {
                        self.send_status(
                            &format!("Phase {}/{}: {}", i + 1, total_phases, call.name),
                            channel_id.as_deref(),
                            thread_id.as_deref(),
                            Some(step_agent_id.as_str()),
                            fallback,
                        )
                        .await;
                    }

                    // Workflow steps run internally — the turn's `code_citations`
                    // have no chat surface here, so they're ignored.
                    // The turn acts as the executing identity (scoped by the
                    // `execute_loop` wrapper) while "workflow-engine" keeps
                    // marking the turn as automated-source.
                    let run_as_for_turn = current_scoped_user();
                    let crate::serve::MessageProgress {
                        progress_rx,
                        handle,
                        cancel_token,
                        ..
                    } = match workforce_common::secret_policy::SECRET_POLICY
                        .scope(
                            secret_policy.clone(),
                            step_service.process_message_with_progress(
                                &channel,
                                "workflow-engine",
                                run_as_for_turn.as_deref(),
                                &call.prompt,
                                None,
                                None,
                                None,
                                None,
                                channel_id.as_deref(),
                                thread_id.as_deref(),
                                None,
                                None,
                                ticket_id.as_deref(),
                                call.max_rounds,
                                resolved_model.as_deref(),
                                exec.context_eviction_interval,
                                Some(exec.context_eviction_keep_last),
                                call.whitelist.as_deref(),
                                exec.persona.as_deref(),
                                None,
                                None,
                                None,
                                None, // workflow steps are never spoken,
                                None,
                            ),
                        )
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            error!(
                                instance_id = %instance_id,
                                step = %exec.step_id,
                                phase = %call.name,
                                agent = %step_agent_id,
                                error = %e,
                                "Step execution failed to start"
                            );
                            self.halt(
                                &mut instance,
                                &e.to_string(),
                                &format!("Workflow failed at step {}: {}", exec.step_id, e),
                                channel_id.as_deref(),
                                thread_id.as_deref(),
                                Some(step_agent_id.as_str()),
                                fallback,
                            )
                            .await?;
                            failed = true;
                            break 'phase_loop;
                        }
                    };

                    self.cancel_tokens
                        .lock()
                        .await
                        .insert(instance_id.to_string(), cancel_token);

                    let progress_surface = self
                        .get_surface_for_agent(Some(step_agent_id.as_str()), fallback)
                        .await;
                    let progress_channel_id = if announce { channel_id.clone() } else { None };
                    let progress_thread_id = thread_id.clone();
                    let progress_step_msg_id = step_msg_id.clone();
                    tokio::spawn(async move {
                        consume_step_progress(
                            progress_rx,
                            progress_surface,
                            progress_channel_id,
                            progress_thread_id,
                            progress_step_msg_id,
                        )
                        .await;
                    });

                    let run_result = match handle.await {
                        Ok(Ok(r)) => r,
                        Ok(Err(e)) => {
                            self.cancel_tokens.lock().await.remove(instance_id);
                            instance.record_failed_attempt(
                                &exec.step_id,
                                &e.to_string(),
                                started_at,
                                total_rounds,
                                step_cost_usd,
                                provenance.clone(),
                            );
                            add_step_reaction(
                                self.get_surface_for_agent(Some(step_agent_id.as_str()), fallback)
                                    .await,
                                channel_id.as_deref(),
                                step_msg_id.as_deref(),
                                channels::indicator::ERROR,
                            )
                            .await;
                            error!(
                                instance_id = %instance_id,
                                step = %exec.step_id,
                                phase = %call.name,
                                agent = %step_agent_id,
                                error = %e,
                                "Step execution failed"
                            );
                            self.halt(
                                &mut instance,
                                &e.to_string(),
                                &format!("Workflow failed at step {}: {}", exec.step_id, e),
                                channel_id.as_deref(),
                                thread_id.as_deref(),
                                Some(step_agent_id.as_str()),
                                fallback,
                            )
                            .await?;
                            failed = true;
                            break 'phase_loop;
                        }
                        Err(e) => {
                            self.cancel_tokens.lock().await.remove(instance_id);
                            add_step_reaction(
                                self.get_surface_for_agent(Some(step_agent_id.as_str()), fallback)
                                    .await,
                                channel_id.as_deref(),
                                step_msg_id.as_deref(),
                                channels::indicator::ERROR,
                            )
                            .await;
                            error!(
                                instance_id = %instance_id,
                                step = %exec.step_id,
                                phase = %call.name,
                                error = %e,
                                "Step task failed"
                            );
                            instance.record_failed_attempt(
                                &exec.step_id,
                                &e.to_string(),
                                started_at,
                                total_rounds,
                                step_cost_usd,
                                provenance.clone(),
                            );
                            WorkflowExecutor::fail(&mut instance, &e.to_string());
                            self.save_instance(&instance)?;
                            failed = true;
                            break 'phase_loop;
                        }
                    };

                    self.cancel_tokens.lock().await.remove(instance_id);

                    if let Some(reason) = run_as_for_turn.as_deref().and_then(|principal| {
                        crate::cap_gate::stopped(
                            step_service.identity_store.read().as_ref(),
                            step_service.capability_gate.read().as_deref(),
                            principal,
                            &run_result.tool_invocations,
                        )
                        .map(|stop| stop.reason())
                    }) {
                        instance.record_failed_attempt(
                            &exec.step_id,
                            &reason,
                            started_at,
                            total_rounds,
                            step_cost_usd,
                            provenance.clone(),
                        );
                        self.halt(
                            &mut instance,
                            &reason,
                            &format!("Workflow failed at step {}: {reason}", exec.step_id),
                            channel_id.as_deref(),
                            thread_id.as_deref(),
                            Some(step_agent_id.as_str()),
                            fallback,
                        )
                        .await?;
                        failed = true;
                        break 'phase_loop;
                    }

                    total_rounds = total_rounds.saturating_add(run_result.rounds);
                    any_budget_capped |= run_result.budget_capped;
                    let usage = run_result.total_usage;
                    provenance.engine = Some(AGENT_ENGINE.into());
                    provenance.model = Some(run_result.model.clone());
                    provenance.input_tokens =
                        Some(provenance.input_tokens.unwrap_or(0) + u64::from(usage.input_tokens));
                    provenance.output_tokens = Some(
                        provenance.output_tokens.unwrap_or(0) + u64::from(usage.output_tokens),
                    );
                    step_cost_usd += step_service.cost_tracker.calculate_cost(
                        &run_result.model,
                        u64::from(usage.input_tokens),
                        u64::from(usage.output_tokens),
                        u64::from(usage.cache_creation_input_tokens),
                        u64::from(usage.cache_read_input_tokens),
                    );
                    response = run_result.response;

                    if i + 1 < total_phases {
                        let fresh = self.load_instance(instance_id)?;
                        if fresh.is_terminal() || fresh.status == InstanceStatus::Paused {
                            info!(
                                instance_id = %instance_id,
                                step = %exec.step_id,
                                phase = %call.name,
                                status = %fresh.status,
                                "Workflow stopped mid-phase — aborting remaining phases"
                            );
                            break 'phase_loop;
                        }
                    }
                }

                if failed {
                    break;
                }

                (response, total_rounds, any_budget_capped)
            };

            // ── Check if workflow was aborted/paused while step was running ──
            // Without this, save_instance() below would overwrite the abort with stale state.
            if self.stopped_during_step(instance_id, &exec.step_id)? {
                break;
            }

            // What the step hands on: its package, once it reads cleanly and
            // any verifier has passed it. A step that is sent back runs again
            // with what was wrong; one that cannot be put right stops the run.
            let step_def = def.get_step(&exec.step_id);
            let assessed = match step_def {
                Some(step) => {
                    assess_handover(
                        step,
                        &exec,
                        &step_service,
                        &instance,
                        &response,
                        self.verifier.get(),
                        &mut step_cost_usd,
                    )
                    .await
                }
                None => Ok(None),
            };
            // A verifier takes its time, and a run aborted or paused while it
            // judged must not be overwritten by the verdict.
            if self.stopped_during_step(instance_id, &exec.step_id)? {
                break;
            }
            let step_outputs = match assessed {
                Ok(package) => package,
                Err(Refusal::SendBack(correction)) => {
                    warn!(
                        instance_id = %instance_id,
                        step = %exec.step_id,
                        cause = ?correction.cause,
                        "Step sent back for correction"
                    );
                    if announce {
                        self.send_status(
                            &format!(
                                "Step '{}' was sent back: {}",
                                step_name,
                                correction.detail.lines().next().unwrap_or_default()
                            ),
                            channel_id.as_deref(),
                            thread_id.as_deref(),
                            Some(step_agent_id.as_str()),
                            fallback,
                        )
                        .await;
                    }
                    let attempted = step_def.and_then(|step| {
                        crate::handoff::attempted(&response, &step.outputs, correction.cause)
                    });
                    instance.record_correction(
                        &exec.step_id,
                        &response,
                        attempted,
                        correction,
                        started_at,
                        actual_rounds,
                        step_cost_usd,
                        budget_capped,
                        provenance.clone(),
                    );
                    coding_round::clear_note(&mut instance);
                    self.save_instance(&instance)?;
                    continue;
                }
                Err(Refusal::Fail(why)) => {
                    let msg = format!("Step '{}' {}", exec.step_id, why);
                    error!(instance_id = %instance_id, step = %exec.step_id, %msg);
                    instance.record_failed_attempt(
                        &exec.step_id,
                        &response,
                        started_at,
                        actual_rounds,
                        step_cost_usd,
                        provenance.clone(),
                    );
                    self.halt(
                        &mut instance,
                        &msg,
                        &Self::stopped_card(step_name, &why),
                        channel_id.as_deref(),
                        thread_id.as_deref(),
                        Some(step_agent_id.as_str()),
                        fallback,
                    )
                    .await?;
                    break;
                }
            };

            // A template report renders against the variables as they will be
            // once this step's outputs merge, so it can read its own outputs.
            // A template that cannot render is an authoring defect, like an
            // invalid output: deterministic, so not retried.
            let step_report = match &exec.report {
                None => None,
                Some(StepReport::Prompted) => parse_report(&response),
                Some(StepReport::Template(template)) => {
                    let visible = with_step_outputs(
                        &instance.variables,
                        &exec.step_id,
                        provenance.exposed(step_outputs.as_ref()).as_ref(),
                    );
                    match workforce_orchestrator::workflow::report::render(template, &visible) {
                        Ok(text) => Some(text),
                        Err(render_err) => {
                            let msg = format!(
                                "Step '{}' report template failed to render: {}",
                                exec.step_id, render_err
                            );
                            error!(instance_id = %instance_id, step = %exec.step_id, %msg);
                            instance.record_failed_attempt(
                                &exec.step_id,
                                &response,
                                started_at,
                                actual_rounds,
                                step_cost_usd,
                                provenance.clone(),
                            );
                            self.halt(
                                &mut instance,
                                &msg,
                                &Self::stopped_card(step_name, &render_err),
                                channel_id.as_deref(),
                                thread_id.as_deref(),
                                Some(step_agent_id.as_str()),
                                fallback,
                            )
                            .await?;
                            break;
                        }
                    }
                }
            };

            // The step record now carries the tool's answer, or the coding
            // round's; their notes have done their job.
            take_reported_tool_result(&mut instance, &exec.step_id);
            coding_round::clear_note(&mut instance);

            // An agent step that ended with no route of its own gets its
            // branch from the decision model, when one is configured.
            let settled = crate::decide::settle_branch(
                &crate::credentials::Caller::in_scope(
                    step_service.vault.clone(),
                    step_service.global_vault.clone(),
                ),
                &crate::decision_records::Subject {
                    instance_id: Some(instance.id.clone()),
                    step_id: Some(exec.step_id.clone()),
                    ..step_service
                        .decision_subject(crate::user_config::current_identity_id(), "workflow")
                },
                &def,
                &exec.step_id,
                &exec.mode,
                crate::decide::Produced {
                    response: &response,
                    outputs: step_outputs.as_ref(),
                    report: step_report.as_deref(),
                },
                &mut provenance,
            )
            .await;

            // Record the step; `complete_step` stamps the outcome it routes on.
            instance.record_step_with_provenance(
                &exec.step_id,
                None,
                &response,
                step_outputs,
                step_report.clone(),
                started_at,
                actual_rounds,
                step_cost_usd,
                budget_capped,
                provenance.clone(),
            );
            if let Some(label) = &settled {
                instance.preset_outcome(&exec.step_id, label, OutcomeBy::DecisionModel);
            }
            if let Some(first) = &first {
                first.preset(&mut instance, &exec.step_id);
            }

            // Mark step as complete with a ✅ reaction
            add_step_reaction(
                self.get_surface_for_agent(Some(step_agent_id.as_str()), fallback)
                    .await,
                channel_id.as_deref(),
                step_msg_id.as_deref(),
                channels::indicator::DONE,
            )
            .await;

            // Deliver step output to the initiating channel
            // (deferred — the transition decides what, if anything, to post)
            let step_output_text = response.clone();
            let report_text = match &exec.report {
                None => None,
                Some(StepReport::Prompted) => Some(ReportText::Block(step_report.as_deref())),
                Some(StepReport::Template(_)) => step_report.as_deref().map(ReportText::Rendered),
            };

            let transition = WorkflowExecutor::complete_step(&def, &mut instance, &response)?;
            if let (Some(first), Some(step)) = (&first, step_def) {
                first.stamp(step, &mut instance);
            }

            // Create a worktree if this step outputs target_repo (enables concurrent isolation)
            if let Some(mgr) = self.worktree_manager.get() {
                let target_repo = instance
                    .variables
                    .get("steps")
                    .and_then(|s| s.as_object())
                    .and_then(|steps| steps.get(&exec.step_id))
                    .and_then(|s| s.as_object())
                    .and_then(|outputs| outputs.get("target_repo"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);

                if let Some(target_repo) = target_repo.as_deref() {
                    let scope_id = format!("wf-{}", instance_id);
                    let check_key = format!("workflow:{}:_check", instance_id);
                    if mgr.resolve_for_session(&check_key, target_repo).is_none() {
                        let source = step_service.workspace.join("repos").join(target_repo);
                        let owner = target_repo.split('/').next().unwrap_or(target_repo);
                        let github_token = crate::user_config::resolve_github_pat(
                            step_service.vault.as_deref(),
                            step_service.global_vault.as_deref(),
                            owner,
                        );
                        if source.exists() {
                            match mgr
                                .create_worktree(
                                    &scope_id,
                                    target_repo,
                                    &source,
                                    github_token.as_deref(),
                                )
                                .await
                            {
                                Ok(path) => {
                                    let path_str = path.display().to_string();
                                    if let Some(vars) = instance.variables.as_object_mut() {
                                        vars.insert(
                                            "_worktree_path".to_string(),
                                            serde_json::json!(path_str),
                                        );
                                    }
                                    info!(
                                        instance_id = %instance_id,
                                        repo = %target_repo,
                                        worktree = %path_str,
                                        "Created worktree for workflow instance"
                                    );
                                }
                                Err(e) => {
                                    warn!(
                                        instance_id = %instance_id,
                                        repo = %target_repo,
                                        error = %e,
                                        "Failed to create worktree, steps will share workspace"
                                    );
                                }
                            }
                        }
                    }
                }
            }

            // Clear human feedback after the step consumes it, so it doesn't
            // leak into unrelated subsequent steps.
            if let Some(vars) = instance.variables.as_object_mut() {
                vars.remove("_human_feedback");
            }

            let opened = match (&transition, step_def) {
                (StepTransition::Paused { .. }, Some(step)) => match &step.decision {
                    Some(spec) => {
                        let body =
                            step_outbound_text(delivery, true, report_text, &step_output_text).map(
                                |message| match message {
                                    StepMessage::Rendered(text) => text.to_string(),
                                    StepMessage::Prose(text) => strip_tool_json(text),
                                },
                            );
                        Some(
                            self.open_decision(&def, step, spec, &instance, body, &step_agent_id)
                                .await,
                        )
                    }
                    None => None,
                },
                _ => None,
            };
            let gate = match opened {
                Some(Ok(decision)) => {
                    decision.write(&mut instance.variables);
                    Some(decision.gate)
                }
                Some(Err(reason)) => {
                    instance.fail_last_record(&exec.step_id);
                    let msg = format!(
                        "Step '{}' could not open its decision: {reason}",
                        exec.step_id
                    );
                    error!(instance_id = %instance_id, step = %exec.step_id, %msg);
                    self.halt(
                        &mut instance,
                        &msg,
                        &Self::stopped_card(step_name, &reason),
                        channel_id.as_deref(),
                        thread_id.as_deref(),
                        Some(step_agent_id.as_str()),
                        fallback,
                    )
                    .await?;
                    break;
                }
                None => None,
            };

            self.save_instance(&instance)?;
            if let (Some(first), Some(step)) = (first, step_def) {
                first.keep(step, &instance);
            }

            // A decision's card is its pause card: posted once, by delivery,
            // and from here the run changes only through fenced updates.
            if let Some(gate) = gate {
                self.deliver_decision(instance_id, gate).await;
                info!(instance_id = %instance_id, step = %exec.step_id, "Workflow waits on a decision");
                break;
            }

            match transition {
                StepTransition::NextStep { step_id } => {
                    self.send_step_outcome(
                        delivery,
                        step_name,
                        false,
                        report_text,
                        &step_output_text,
                        channel_id.as_deref(),
                        thread_id.as_deref(),
                        Some(step_agent_id.as_str()),
                        fallback,
                    )
                    .await;
                    info!(
                        instance_id = %instance_id,
                        next_step = %step_id,
                        "Advancing to next step"
                    );
                }
                StepTransition::Paused { reason } => {
                    let report_msg_id = self
                        .send_step_outcome(
                            delivery,
                            step_name,
                            true,
                            report_text,
                            &step_output_text,
                            channel_id.as_deref(),
                            thread_id.as_deref(),
                            Some(step_agent_id.as_str()),
                            fallback,
                        )
                        .await;
                    let pause_msg = if reason.starts_with("awaiting") {
                        format!(
                            "*Approval required* — step *{}*\n\
                             Reason: {}\n\n\
                             `approve` · `reject <feedback>` · `status` · `stop`",
                            step_name, reason
                        )
                    } else {
                        format!(
                            "Workflow paused: {} — `approve` · `reject` · `status`",
                            reason
                        )
                    };
                    let pause_msg_id = self
                        .send_status(
                            &pause_msg,
                            channel_id.as_deref(),
                            thread_id.as_deref(),
                            Some(step_agent_id.as_str()),
                            fallback,
                        )
                        .await;
                    // Reports delivery runs unanchored (no banner). The pause
                    // card becomes the thread root so approve/reject replies
                    // route back to this instance.
                    if thread_id.is_none() {
                        if let Some(anchor) = pause_msg_id.or(report_msg_id) {
                            info!(instance_id = %instance_id, thread_id = %anchor, "Anchoring workflow to Slack thread");
                            self.anchor_thread(instance_id, channel_id.as_deref(), &anchor);
                        }
                    }
                    info!(
                        instance_id = %instance_id,
                        reason = %reason,
                        "Workflow paused"
                    );
                    break;
                }
                StepTransition::Done => {
                    self.send_step_outcome(
                        delivery,
                        step_name,
                        false,
                        report_text,
                        &step_output_text,
                        channel_id.as_deref(),
                        thread_id.as_deref(),
                        Some(step_agent_id.as_str()),
                        fallback,
                    )
                    .await;
                    if announce {
                        self.send_status(
                            "Workflow complete",
                            channel_id.as_deref(),
                            thread_id.as_deref(),
                            None,
                            fallback,
                        )
                        .await;
                    }
                    info!(instance_id = %instance_id, "Workflow completed");
                    break;
                }
                StepTransition::Exit {
                    handoff_type,
                    summary,
                } => {
                    self.send_step_outcome(
                        delivery,
                        step_name,
                        false,
                        report_text,
                        &step_output_text,
                        channel_id.as_deref(),
                        thread_id.as_deref(),
                        Some(step_agent_id.as_str()),
                        fallback,
                    )
                    .await;
                    if announce {
                        self.send_status(
                            &format!("Workflow exited: {}", handoff_type),
                            channel_id.as_deref(),
                            thread_id.as_deref(),
                            None,
                            fallback,
                        )
                        .await;
                    }
                    info!(
                        instance_id = %instance_id,
                        exit_type = %handoff_type,
                        summary = %workforce_common::text::truncate_str(&summary, 100),
                        "Workflow exited"
                    );
                    break;
                }
                StepTransition::NoRoute { step_id, outcome } => {
                    self.send_step_outcome(
                        delivery,
                        step_name,
                        false,
                        report_text,
                        &step_output_text,
                        channel_id.as_deref(),
                        thread_id.as_deref(),
                        Some(step_agent_id.as_str()),
                        fallback,
                    )
                    .await;
                    let msg = format!(
                        "Routing error: step '{}' got unknown outcome '{}'",
                        step_id, outcome
                    );
                    self.halt(
                        &mut instance,
                        &msg,
                        &format!("Workflow failed: {}", msg),
                        channel_id.as_deref(),
                        thread_id.as_deref(),
                        None,
                        fallback,
                    )
                    .await?;
                    error!(instance_id = %instance_id, %msg, "Workflow routing error");
                    break;
                }
            }
        }

        // Clean up worktree for terminal workflows (paused ones keep theirs for resume)
        let final_instance = self.load_instance(instance_id).ok();
        let should_cleanup = final_instance
            .as_ref()
            .map(|i| i.is_terminal())
            .unwrap_or(false);
        if should_cleanup {
            self.cleanup_worktree(instance_id).await;
        }

        Ok(())
    }

    /// Clean up the workspaces a workflow instance used: its local worktree,
    /// and the scope its `claude-code` steps last ran in on the coding worker.
    /// A scope an earlier step prepared before any step named a repo is left to
    /// the worker's own lifetime.
    async fn cleanup_worktree(&self, instance_id: &str) {
        let scope_id = format!("wf-{}", instance_id);
        if let Some(mgr) = self.worktree_manager.get() {
            if let Err(e) = mgr.remove_worktree(&scope_id).await {
                warn!(
                    instance_id = %instance_id,
                    error = %e,
                    "Failed to clean up worktree for workflow instance"
                );
            }
        }
        let Some(launcher) = self.remote_launcher.get() else {
            return;
        };
        let Ok(instance) = self.load_instance(instance_id) else {
            return;
        };
        let agent_id = self.instance_agent(&instance);
        let project = step_project_name(Self::target_repo(&instance));
        if let Err(e) = launcher.close(agent_id, &project, &scope_id).await {
            warn!(
                instance_id = %instance_id,
                error = %e,
                "Failed to release the workflow's workspace on the coding worker"
            );
        }
    }

    /// Whether the run was stopped or paused while `step` ran, reloaded from
    /// the store, so the step's result is not the run's to keep. A pause
    /// drops the step's noted round: the step runs anew once resumed.
    fn stopped_during_step(&self, instance_id: &str, step: &str) -> Result<bool> {
        let mut fresh = self.load_instance(instance_id)?;
        if fresh.is_terminal() {
            info!(
                instance_id = %instance_id,
                step = %step,
                status = %fresh.status,
                "Workflow was stopped during step execution — discarding step result"
            );
            return Ok(true);
        }
        if fresh.status != InstanceStatus::Paused {
            return Ok(false);
        }
        info!(
            instance_id = %instance_id,
            step = %step,
            "Workflow was paused during step execution — discarding step result"
        );
        if coding_round::clear_note(&mut fresh) {
            self.save_instance(&fresh)?;
        }
        Ok(true)
    }

    /// End the job a step's noted round last reached on the coding worker,
    /// wherever the loop running it is, if one still is.
    async fn end_noted_job(&self, instance: &WorkflowInstance) {
        let (Some(launcher), Some(job)) = (
            self.remote_launcher.get(),
            coding_round::noted_job(instance),
        ) else {
            return;
        };
        let project = step_project_name(Self::target_repo(instance));
        launcher
            .end_job(
                self.instance_agent(instance),
                &project,
                &format!("wf-{}", instance.id),
                &job,
            )
            .await;
    }

    /// The agent a workflow instance's workspaces belong to.
    fn instance_agent<'a>(&'a self, instance: &'a WorkflowInstance) -> &'a str {
        instance
            .agent_id
            .as_deref()
            .unwrap_or(self.service.agent_id.as_str())
    }

    fn save_instance(
        &self,
        instance: &workforce_orchestrator::workflow::instance::WorkflowInstance,
    ) -> Result<()> {
        let row = row_of(instance);
        // Inside our own execute loop the save is fenced on the execution
        // lease; a fenced-out write means another instance took over — stop
        // instead of clobbering its progress (last-writer-wins corruption).
        if self.active_loops.lock().contains(&instance.id) {
            let saved = self
                .telemetry_store
                .save_workflow_instance_owned(&row, process_instance_id())?;
            if !saved {
                return Err(eyre!(
                    "{FENCED} workflow {} is now owned by another instance",
                    instance.id
                ));
            }
            return Ok(());
        }
        self.telemetry_store.save_workflow_instance(&row)
    }

    /// The run `id` with the version it was read at.
    fn load_fenced(&self, id: &str) -> Result<(WorkflowInstance, i64)> {
        let row = self
            .telemetry_store
            .get_workflow_instance(id)?
            .ok_or_else(|| eyre!("Workflow instance '{}' not found", id))?;
        let version = row.version;
        Ok((instance_of(row)?, version))
    }

    /// Change the run `id` from outside its execute loop: apply `change` to
    /// it as stored and save only at the version it was read at, reading it
    /// afresh and applying `change` again when another write got there first.
    /// `change` re-checks everything it relies on, every time it runs.
    pub(crate) fn update_instance<T, E: From<eyre::Report>>(
        &self,
        id: &str,
        mut change: impl FnMut(&mut WorkflowInstance) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        for _ in 0..FENCE_ATTEMPTS {
            let (mut instance, version) = self.load_fenced(id)?;
            let changed = change(&mut instance)?;
            if self
                .telemetry_store
                .save_workflow_instance_fenced(&row_of(&instance), version)?
            {
                return Ok(changed);
            }
        }
        Err(eyre!("workflow {id} kept changing; nothing was changed").into())
    }

    fn load_instance(
        &self,
        id: &str,
    ) -> Result<workforce_orchestrator::workflow::instance::WorkflowInstance> {
        let row = self
            .telemetry_store
            .get_workflow_instance(id)?
            .ok_or_else(|| eyre!("Workflow instance '{}' not found", id))?;

        instance_of(row)
    }

    /// Get the correct surface to use for a given agent.
    /// Prefers the per-agent surface (correct bot token), then tries the
    /// fallback agent (e.g. the originating agent for DM delivery), then
    /// falls back to the first surface in the registry.
    async fn get_surface_for_agent(
        &self,
        agent_id: Option<&str>,
        fallback_agent_id: Option<&str>,
    ) -> Option<Arc<dyn ChannelSurface>> {
        // Per-agent surface first (correct bot token for this agent), then the
        // originating/fallback agent's (for DM delivery).
        let per_agent = {
            let surfaces = self.agent_surfaces.read();
            [agent_id, fallback_agent_id]
                .into_iter()
                .flatten()
                .find_map(|aid| surfaces.get(aid).cloned())
        };
        if per_agent.is_some() {
            return per_agent;
        }
        let registry_ref = self
            .surface_registry_override
            .get()
            .unwrap_or(&self.service.surface_registry);
        let registry = registry_ref.read().await;
        let surface = registry.all_for("slack").into_iter().next();
        if surface.is_none() {
            warn!(
                agent_id = ?agent_id,
                fallback_agent_id = ?fallback_agent_id,
                "No surface found for workflow status delivery — workflow will run dark"
            );
        }
        surface
    }

    async fn check_human_gate(
        &self,
        instance: &WorkflowInstance,
        originating_agent_id: &Option<String>,
    ) -> Result<()> {
        if originating_agent_id.is_some() && instance.status == InstanceStatus::Paused {
            if let Some(ref step_id) = instance.current_step_id {
                let is_awaiting = self
                    .definitions
                    .read()
                    .await
                    .get(&instance.workflow_id)
                    .and_then(|o| o.def.get_step(step_id))
                    .filter(|step| step.decision.is_none())
                    .and_then(|step| step.pause.as_ref())
                    .map(|reason| reason.starts_with("awaiting"))
                    .unwrap_or(false);
                if is_awaiting {
                    return Err(eyre!(
                        "Cannot modify workflow '{}' — paused for human approval. \
                         Only explicit human commands (approve/reject) can resume it.",
                        instance.id
                    ));
                }
            }
        }
        Ok(())
    }

    async fn announce_workflow_plan(
        &self,
        def: &WorkflowDefinition,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        agent_id: Option<&str>,
        fallback_agent_id: Option<&str>,
    ) -> Option<String> {
        let has_pause_gates = def.steps.iter().any(|s| s.pause.is_some());

        let step_lines: Vec<String> = def
            .steps
            .iter()
            .enumerate()
            .map(|(i, step)| {
                let marker = if step.pause.is_some() { " [pause]" } else { "" };
                format!("{}. {}{}", i + 1, step.name, marker)
            })
            .collect();

        let mut parts = vec![format!("*Workflow: {}*", def.workflow.name)];
        if !def.workflow.description.is_empty() {
            parts.push(def.workflow.description.clone());
        }
        parts.push(String::new());
        parts.extend(step_lines);
        parts.push(String::new());
        parts.push("Commands: `status` · `stop`".to_string());
        if has_pause_gates {
            parts.push("At pause gates: `approve` · `reject <feedback>`".to_string());
        }

        self.send_status(
            &parts.join("\n"),
            channel_id,
            thread_id,
            agent_id,
            fallback_agent_id,
        )
        .await
    }

    /// The hub tool patterns a Claude Code step is served.
    fn step_hub_tools(&self) -> Vec<String> {
        self.mcp_exposed_tools.get().cloned().unwrap_or_default()
    }

    /// Resolve the working directory for a workflow step's subprocess.
    ///
    /// Order of preference:
    /// 1. `_worktree_path` (set by the worktree manager when a prior step
    ///    emitted `target_repo` and a worktree was successfully created).
    /// 2. `<workspace>/repos/<target_repo>` — the actual repo directory, used
    ///    when worktree creation was skipped or failed but we still know
    ///    which repo the workflow is targeting. Without this fallback the
    ///    subprocess would land in `<workspace>/repos` which is not a git
    ///    repo and breaks every git/cargo/relative-path operation.
    /// 3. `<workspace>/repos` — last-resort fallback for workflows with no
    ///    repo context (legacy behaviour).
    fn resolve_step_workdir(
        instance: &WorkflowInstance,
        workspace: &std::path::Path,
    ) -> std::path::PathBuf {
        if let Some(wt) = instance
            .variables
            .get("_worktree_path")
            .and_then(|v| v.as_str())
        {
            return std::path::PathBuf::from(wt);
        }
        if let Some(target_repo) = Self::target_repo(instance) {
            let candidate = workspace.join("repos").join(target_repo);
            if candidate.exists() {
                return candidate;
            }
        }
        workspace.join("repos")
    }

    /// The `owner/repo` a prior step named as its `target_repo` output, if any.
    fn target_repo(instance: &WorkflowInstance) -> Option<&str> {
        step_output(&instance.variables, "target_repo")
    }

    /// The `owner/repo` list a prior step named as its `workspace_repos`
    /// output, if any.
    fn workspace_repos(instance: &WorkflowInstance) -> Option<&str> {
        step_output(&instance.variables, "workspace_repos")
    }

    /// The workspace a remote `claude-code` step runs in: the repos prior steps
    /// named as `target_repo` and `workspace_repos`, checked out on the coding
    /// worker under a scope every step of this instance shares and started in
    /// the target, or an empty scope when no step has named a target yet.
    async fn remote_step_sandbox(
        &self,
        launcher: &Arc<dyn SandboxLauncher>,
        instance: &WorkflowInstance,
        step_service: &Arc<AgentService>,
    ) -> Result<Arc<dyn CodingSandbox>> {
        let agent_id = self.instance_agent(instance);
        let project = step_project(
            agent_id,
            Self::target_repo(instance),
            Self::workspace_repos(instance),
        )?;
        let tokens: HashMap<String, String> = project
            .repos
            .iter()
            .filter_map(|repo| {
                let workforce_projects::RepoSource::Github { owner, .. } = &repo.source;
                crate::user_config::resolve_github_pat(
                    step_service.vault.as_deref(),
                    step_service.global_vault.as_deref(),
                    owner,
                )
                .map(|token| (owner.clone(), token))
            })
            .collect();
        launcher
            .ensure(&WorkspaceSpec {
                agent_id,
                project: &project,
                scope: &format!("wf-{}", instance.id),
                tokens: &tokens,
                home: HomeSpec::Ephemeral,
                primary: project.repos.first().map(|repo| repo.alias.as_str()),
            })
            .await
    }

    /// Re-materialise the workflow's worktree on resume/jump if it was destroyed
    /// by a prior failure or cancellation. Without this, jumps from `escalate`
    /// → `merge` execute in a non-existent cwd and silently produce wrong
    /// results. Always returns Ok — downstream steps surface concrete failures.
    async fn ensure_worktree_alive(&self, instance_id: &str) {
        let Some(mgr) = self.worktree_manager.get() else {
            return;
        };
        let Ok(instance) = self.load_instance(instance_id) else {
            return;
        };

        let Some(target_repo) = Self::target_repo(&instance).map(str::to_string) else {
            return;
        };

        if let Some(p) = instance
            .variables
            .get("_worktree_path")
            .and_then(|v| v.as_str())
        {
            if std::path::Path::new(p).exists() {
                return;
            }
            warn!(
                instance_id = %instance_id,
                worktree = %p,
                "Recorded worktree path no longer exists on disk — will recreate"
            );
        }

        let agent_service = instance
            .agent_id
            .as_deref()
            .and_then(|aid| self.agent_registry.get().and_then(|reg| reg.get(aid)));
        let workspace = agent_service
            .as_ref()
            .map(|svc| svc.workspace.clone())
            .unwrap_or_else(|| self.service.workspace.clone());
        let source = workspace.join("repos").join(&target_repo);
        if !source.exists() {
            warn!(
                instance_id = %instance_id,
                repo = %target_repo,
                source = %source.display(),
                "Source clone missing — cannot recreate worktree on resume"
            );
            return;
        }

        let owner = target_repo
            .split('/')
            .next()
            .unwrap_or(target_repo.as_str());
        let agent_vault = agent_service
            .as_ref()
            .and_then(|svc| svc.vault.as_deref())
            .or(self.service.vault.as_deref());
        let global_vault = agent_service
            .as_ref()
            .and_then(|svc| svc.global_vault.as_deref())
            .or(self.service.global_vault.as_deref());
        let github_token = crate::user_config::resolve_github_pat(agent_vault, global_vault, owner);
        let scope_id = format!("wf-{}", instance_id);
        match mgr
            .create_worktree(&scope_id, &target_repo, &source, github_token.as_deref())
            .await
        {
            Ok(path) => {
                let path_str = path.display().to_string();
                if let Err(e) = self.set_instance_variable(instance_id, "_worktree_path", &path_str)
                {
                    warn!(error = %e, "Failed to store recreated worktree path on instance");
                }
                info!(
                    instance_id = %instance_id,
                    repo = %target_repo,
                    worktree = %path_str,
                    "Recreated worktree for resumed workflow instance"
                );
            }
            Err(e) => {
                warn!(
                    instance_id = %instance_id,
                    repo = %target_repo,
                    error = %e,
                    "Failed to recreate worktree on resume — downstream step will likely fail"
                );
            }
        }
    }

    /// Post a human-facing summary to the originating Slack thread when a
    /// workflow reaches a terminal state. Without this the only signal of
    /// failure is a server-side `error!` log invisible to the user.
    async fn post_terminal_status(
        &self,
        instance_id: &str,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        outcome: TerminalOutcome<'_>,
    ) {
        let instance = self.load_instance(instance_id).ok();
        // A run anchors its thread mid-flight (`_thread_id` in instance
        // variables), but the error path that reaches here still holds the
        // caller's pre-anchor ids — for a scheduled run that is `None`.
        // Prefer the caller's ids, then what the instance recorded: a
        // terminal card belongs in the run's thread, not loose in the
        // channel.
        let var_str = |key: &str| {
            instance
                .as_ref()
                .and_then(|i| i.variables.get(key))
                .and_then(|v| v.as_str())
        };
        let channel_id = channel_id.or_else(|| var_str("_channel_id"));
        let thread_id = thread_id.or_else(|| var_str("_thread_id"));
        let desk = instance.as_ref().and_then(desk_of);
        if channel_id.is_none() && desk.is_none() {
            return;
        }
        let text = render_terminal_status(instance_id, instance.as_ref(), &outcome);
        let agent_id = instance.as_ref().and_then(|i| i.agent_id.as_deref());
        // A run can end outside its loop (a failed start), where neither its
        // person nor its Desk conversation is in scope: both are its own.
        let principal = instance
            .as_ref()
            .map(instance_principal)
            .unwrap_or_default();
        let desk = desk
            .zip(principal.run_as.clone())
            .map(|(target, owner)| DeskScope {
                target,
                owner,
                attended: false,
                relayed: false,
            });
        self.run_scoped(
            principal,
            workforce_agent::runner::with_desk(
                desk,
                self.send_status(&text, channel_id, thread_id, agent_id, agent_id),
            ),
        )
        .await;
    }

    /// Stop the run: fail the instance, persist that before anything else can
    /// observe it, then post `card`. When the failure is the hub's own
    /// restart, the instance is instead left as it is — running, at this
    /// step — for the successor's recovery to re-run, exactly as a step
    /// boundary would be, and the channel is told so. A workflow that opted
    /// into `pause_on_usage_cap` is parked the same way when the refusal is a
    /// spent subscription window, and the maintenance sweep resumes it.
    async fn halt(
        &self,
        instance: &mut WorkflowInstance,
        reason: &str,
        card: &str,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        agent_id: Option<&str>,
        fallback_agent_id: Option<&str>,
    ) -> Result<()> {
        if self.interrupted_by_restart(reason).await {
            let step = current_or_last_step(instance).unwrap_or("unknown");
            info!(
                instance_id = %instance.id,
                step = %step,
                error = %reason,
                "Step interrupted by the hub restarting — left for recovery to resume"
            );
            self.send_status(
                &format!(
                    "⏸️ Step '{step}' was interrupted by a hub restart — it resumes on the new hub"
                ),
                channel_id,
                thread_id,
                agent_id,
                fallback_agent_id,
            )
            .await;
            return Ok(());
        }
        self.end_noted_job(instance).await;
        coding_round::clear_note(instance);
        // Held only when the suspension is what stopped the step, as a
        // usage-cap hold is only for a spent window: any other failure fails
        // the run as before, so reactivation does not rerun what broke.
        if self
            .held_person(instance)
            .is_some_and(|person| reason.contains(&crate::standing::refusal(&person)))
        {
            park_for_suspension(instance);
            self.save_instance(instance)?;
            info!(
                instance_id = %instance.id,
                step = current_or_last_step(instance).unwrap_or("unknown"),
                "The person this run acts as is suspended — run held, and resumes when they are not"
            );
            return Ok(());
        }
        if self.pauses_on_usage_cap(&instance.workflow_id).await && is_usage_capped(reason) {
            let step = current_or_last_step(instance)
                .unwrap_or("unknown")
                .to_string();
            park_for_usage_cap(instance, reason);
            self.save_instance(instance)?;
            info!(
                instance_id = %instance.id,
                step = %step,
                "Subscription usage window is spent — run held, and resumes when it clears"
            );
            self.send_status(
                &format!(
                    "⏸️ Step '{step}' held: the subscription's usage window is spent. \
                     The run resumes by itself once it clears."
                ),
                channel_id,
                thread_id,
                agent_id,
                fallback_agent_id,
            )
            .await;
            return Ok(());
        }
        WorkflowExecutor::fail(instance, reason);
        self.save_instance(instance)?;
        self.announce_failure(
            instance,
            reason,
            card,
            channel_id,
            thread_id,
            agent_id,
            fallback_agent_id,
        )
        .await;
        Ok(())
    }

    /// Post the failure card of the failed run `instance`.
    #[allow(clippy::too_many_arguments)]
    async fn announce_failure(
        &self,
        instance: &WorkflowInstance,
        reason: &str,
        card: &str,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        agent_id: Option<&str>,
        fallback_agent_id: Option<&str>,
    ) {
        let card = {
            let definitions = self.definitions.read().await;
            let meta = definitions
                .get(&instance.workflow_id)
                .map(|o| &o.def.workflow);
            failure_card(meta, instance, reason, card)
        };
        self.send_status(&card, channel_id, thread_id, agent_id, fallback_agent_id)
            .await;
    }

    fn stopped_card(step_name: &str, reason: &str) -> String {
        format!(
            "Workflow stopped at step '{step_name}': {reason}\n\n\
             The workflow has been halted. Please investigate and retry."
        )
    }

    /// Post a status text to the channel, in as many messages as it takes,
    /// returning the first message's id. A long report goes out as an ordered
    /// run of whole messages in one thread: the caller's, or — when there was
    /// none — the first message's own, so the run stays together. A later
    /// message that fails ends the run; what was posted stays up.
    async fn send_status(
        &self,
        text: &str,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        agent_id: Option<&str>,
        fallback_agent_id: Option<&str>,
    ) -> Option<String> {
        let (cid, thread_id) = match destination_of(
            channel_id,
            thread_id,
            workforce_agent::runner::current_desk(),
        ) {
            Destination::Slack { channel, thread } => (channel, thread),
            Destination::Desk(scope) => {
                self.say_on_desk(&scope, text, agent_id, fallback_agent_id)
                    .await;
                return None;
            }
            Destination::Dark => return None,
        };
        let surface = self
            .get_surface_for_agent(agent_id, fallback_agent_id)
            .await?;

        tracing::debug!(
            channel = cid,
            agent_id = ?agent_id,
            fallback_agent_id = ?fallback_agent_id,
            platform = %surface.platform(),
            "Sending workflow status"
        );

        // Scrub secrets at the boundary. Any path that builds Slack-bound text
        // (status, failure reports, review summaries) flows through here, so
        // a single redaction gate is enough — see crates/hub/src/redact.rs.
        let redacted_text = crate::redact::redact_secrets(text);
        let chunks = crate::slack_manager::split_for_delivery(&redacted_text);
        let parts = chunks.len();
        let mut chunks = chunks.into_iter();
        let message = |text: &str, thread: Option<&str>| OutboundMessage {
            channel: cid.to_string(),
            text: text.to_string(),
            thread_id: thread.map(str::to_string),
            username: None,
            icon_url: None,
            blocks: None,
        };

        let first =
            match send_status_message(surface.as_ref(), &message(&chunks.next()?, thread_id)).await
            {
                Ok(id) => id,
                Err(e) => {
                    warn!(
                        platform = %surface.platform(),
                        channel = cid,
                        parts,
                        error = %e,
                        "Failed to deliver workflow status"
                    );
                    return None;
                }
            };
        let thread = thread_id.unwrap_or(&first);
        for (chunk, part) in chunks.zip(2..) {
            if let Err(e) =
                send_status_message(surface.as_ref(), &message(&chunk, Some(thread))).await
            {
                warn!(
                    platform = %surface.platform(),
                    channel = cid,
                    part,
                    parts,
                    error = %e,
                    "Workflow status delivery stopped part-way; the rest of the report was not posted"
                );
                break;
            }
        }
        Some(first)
    }

    /// Say `text` in the Desk conversation the run reports to (`scope`), to
    /// its person, in the conversation with the agent that started it.
    /// Desk's surface is append-only: each says once what Slack would show
    /// by editing a card.
    async fn say_on_desk(
        &self,
        scope: &DeskScope,
        text: &str,
        agent_id: Option<&str>,
        fallback_agent_id: Option<&str>,
    ) {
        let service = self
            .agent_registry
            .get()
            .and_then(|registry| {
                [fallback_agent_id, agent_id]
                    .into_iter()
                    .flatten()
                    .find_map(|id| registry.get(id))
            })
            .unwrap_or_else(|| Arc::clone(&self.service));
        if let Err(e) = crate::desk_surface::deliver(
            &service,
            &scope.owner,
            &scope.target,
            crate::desk_surface::Delivery::text(
                DeliveryKind::Workflow,
                crate::redact::redact_secrets(text),
            ),
        )
        .await
        {
            warn!(error = %e, "Failed to deliver workflow status to Desk");
        }
    }

    /// Deliver a completed step's channel-facing text per the delivery mode.
    ///
    /// `Full` posts the response under the step's name (the DW-458 progress
    /// contract). `Reports` posts a declared report bare — it is the
    /// workflow's message to the channel, not progress under a banner — and
    /// returns the posted message id so a pause can anchor its thread to it.
    #[allow(clippy::too_many_arguments)]
    async fn send_step_outcome(
        &self,
        delivery: DeliveryMode,
        step_name: &str,
        paused: bool,
        report: Option<ReportText<'_>>,
        response: &str,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        agent_id: Option<&str>,
        fallback_agent_id: Option<&str>,
    ) -> Option<String> {
        match step_outbound_text(delivery, paused, report, response)? {
            StepMessage::Rendered(text) => {
                if text.trim().is_empty() {
                    return None;
                }
                self.send_status(text, channel_id, thread_id, agent_id, fallback_agent_id)
                    .await
            }
            StepMessage::Prose(text) => match delivery {
                DeliveryMode::Full => {
                    self.send_step_output(
                        step_name,
                        text,
                        channel_id,
                        thread_id,
                        agent_id,
                        fallback_agent_id,
                    )
                    .await;
                    None
                }
                DeliveryMode::Reports => {
                    // The fallback for a declared report whose model omitted the
                    // block is the raw step response — strip embedded tool JSON
                    // as Full mode does, and stay silent when nothing else
                    // remains: a JSON wall is the noise this mode exists to kill.
                    let cleaned = strip_tool_json(text);
                    if cleaned.trim().is_empty() {
                        return None;
                    }
                    self.send_status(&cleaned, channel_id, thread_id, agent_id, fallback_agent_id)
                        .await
                }
            },
        }
    }

    /// Deliver the agent's step output to the initiating channel.
    ///
    /// Strips embedded JSON tool results from the response before sending,
    /// so users see clean prose instead of raw `{"exit_code":0,...}` blocks.
    async fn send_step_output(
        &self,
        step_name: &str,
        response: &str,
        channel_id: Option<&str>,
        thread_id: Option<&str>,
        agent_id: Option<&str>,
        fallback_agent_id: Option<&str>,
    ) {
        if response.trim().is_empty() {
            return;
        }

        let cleaned = strip_tool_json(response);
        if cleaned.trim().is_empty() {
            return;
        }

        let text = format!("**{}**\n\n{}", step_name, cleaned);
        let cid = match destination_of(
            channel_id,
            thread_id,
            workforce_agent::runner::current_desk(),
        ) {
            Destination::Slack { channel, .. } => channel,
            Destination::Desk(scope) => {
                self.say_on_desk(&scope, &text, agent_id, fallback_agent_id)
                    .await;
                return;
            }
            Destination::Dark => return,
        };
        let redacted = crate::redact::redact_secrets(&text);

        if let Some(surface) = self
            .get_surface_for_agent(agent_id, fallback_agent_id)
            .await
        {
            if let Err(e) = surface
                .send_message(&OutboundMessage {
                    channel: cid.to_string(),
                    text: redacted,
                    thread_id: thread_id.map(|t| t.to_string()),
                    username: None,
                    icon_url: None,

                    blocks: None,
                })
                .await
            {
                warn!(
                    platform = %surface.platform(),
                    error = %e,
                    "Failed to deliver workflow step output"
                );
            }
        }
    }
}

pub(crate) enum TerminalOutcome<'a> {
    Failed { error: &'a str },
    Cancelled { reason: &'a str },
}

/// Render an actionable Slack-bound failure message for a claude-pilot step
/// that exited non-zero. Surfaces the specific phase that failed (looked up
/// from the step definition), a cost+turn summary, and the tail of the
/// pilot's text output where the real diagnostic usually lives.
fn format_pilot_failure_message(
    step_id: &str,
    step_def: Option<&workforce_orchestrator::workflow::definition::StepDefinition>,
    result: &crate::claude_pilot::PilotResult,
    instance_id: &str,
) -> String {
    let failed_phase = step_def
        .and_then(|s| s.phases.get(result.phases_completed as usize))
        .map(|p| p.name.as_str())
        .unwrap_or("unknown");

    let phase_budget = step_def
        .and_then(|s| s.phases.get(result.phases_completed as usize))
        .map(|p| p.max_turns);

    let likely_cause = match phase_budget {
        Some(b) if result.num_turns >= b => format!(
            "exceeded the phase turn budget ({b}) — bump `max_turns` on this phase and retry"
        ),
        Some(_) => "a tool call (lint, typecheck, test, git, gh) returned non-zero and the \
             model could not recover within the budget"
            .to_string(),
        None => "tool call returned non-zero".to_string(),
    };

    let tail = tail_chars(&result.text, 1500);

    format!(
        ":x: *Workflow failed* at step `{step_id}` phase `{failed_phase}`\n\
         • *Phases:* {}/{} completed\n\
         • *Turns:* {} (cost ${:.4})\n\
         • *Likely cause:* {likely_cause}\n\
         • *Instance:* `{instance_id}`\n\n\
         *Last output from the phase:*\n```\n{tail}\n```\n\n\
         _Next: check the PR branch manually or bump the phase budget and re-run._",
        result.phases_completed, result.phases_total, result.num_turns, result.cost_usd,
    )
}

fn tail_chars(s: &str, n: usize) -> String {
    let len = s.chars().count();
    if len <= n {
        return s.to_string();
    }
    s.chars().skip(len - n).collect()
}

fn render_terminal_status(
    instance_id: &str,
    instance: Option<&WorkflowInstance>,
    outcome: &TerminalOutcome<'_>,
) -> String {
    let workflow_id = instance.map(|i| i.workflow_id.as_str()).unwrap_or("?");
    let last_step = instance.and_then(current_or_last_step).unwrap_or("unknown");

    match outcome {
        TerminalOutcome::Failed { error } => {
            format!(
                ":x: *Workflow failed* — `{workflow_id}`\n\
                 • *Step:* `{last_step}`\n\
                 • *Instance:* `{instance_id}`\n\
                 • *Error:* {error}\n\n\
                 _Inspect with `workflow status instance_id={instance_id}` or start a fresh run._"
            )
        }
        TerminalOutcome::Cancelled { reason } => {
            format!(
                ":no_entry: *Workflow cancelled* — `{workflow_id}`\n\
                 • *Step:* `{last_step}`\n\
                 • *Instance:* `{instance_id}`\n\
                 • *Reason:* {reason}"
            )
        }
    }
}

/// Verify the trigger payload matches the workflow's declared context schema.
///
/// Rejects three classes of mistakes the originating agent typically makes:
///   1. Missing `required = true` fields (e.g. workflow needs `repo`, none sent).
///   2. Empty/whitespace values for required fields.
///   3. Unknown field names (e.g. agent sends `workspace` instead of `repo`).
///
/// The error message embeds the workflow's full context schema so the agent
/// can self-correct on retry without a separate `workflow.get` round-trip.
fn validate_required_context(def: &WorkflowDefinition, payload: &serde_json::Value) -> Result<()> {
    let payload_obj = payload.as_object();
    let declared: std::collections::HashSet<&str> =
        def.workflow.context.keys().map(String::as_str).collect();

    let missing: Vec<&str> = def
        .workflow
        .context
        .iter()
        .filter(|(_, field)| field.required)
        .filter(|(name, _)| {
            payload_obj
                .and_then(|o| o.get(name.as_str()))
                .map(value_is_empty)
                .unwrap_or(true)
        })
        .map(|(name, _)| name.as_str())
        .collect();

    let provided: Vec<&str> = payload_obj
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();

    let unknown: Vec<&str> = provided
        .iter()
        .copied()
        .filter(|k| !declared.contains(k) && !RESERVED_PAYLOAD_KEYS.contains(k))
        .collect();

    if missing.is_empty() && unknown.is_empty() {
        return Ok(());
    }

    Err(eyre!(format_validation_error(
        def, &missing, &unknown, &provided
    )))
}

fn format_validation_error(
    def: &WorkflowDefinition,
    missing: &[&str],
    unknown: &[&str],
    provided: &[&str],
) -> String {
    let mut msg = format!("Workflow '{}' rejected.", def.workflow.id);

    if !missing.is_empty() {
        msg.push_str(&format!("\nRequired field(s) missing: {missing:?}."));
    }

    if !unknown.is_empty() {
        msg.push_str("\nUnknown field(s):");
        for k in unknown {
            match suggest_field(k, &def.workflow.context) {
                Some(hint) => msg.push_str(&format!(" '{k}' (did you mean '{hint}'?)")),
                None => msg.push_str(&format!(" '{k}'")),
            }
        }
        msg.push('.');
    }

    msg.push_str("\n\nWorkflow context schema:");
    let mut entries: Vec<_> = def.workflow.context.iter().collect();
    entries.sort_by_key(|(k, _)| k.as_str());
    for (name, field) in entries {
        let req = if field.required {
            "required"
        } else {
            "optional"
        };
        let desc = if field.description.is_empty() {
            String::new()
        } else {
            format!(": {}", field.description)
        };
        msg.push_str(&format!("\n  - {name} ({req}){desc}"));
    }

    if !provided.is_empty() {
        msg.push_str(&format!("\n\nYou provided: {provided:?}"));
    }

    msg
}

/// Suggest the closest declared field name for an unknown key, if one is
/// within edit-distance ≤ 3 (handles `workspace`→`repo` only via the second
/// shared-substring pass; `workskace`→`workspace` would match by distance).
fn suggest_field(
    unknown: &str,
    context: &std::collections::HashMap<
        String,
        workforce_orchestrator::workflow::definition::ContextField,
    >,
) -> Option<String> {
    let unknown_lower = unknown.to_lowercase();
    context
        .keys()
        .map(|k| (k, levenshtein(&unknown_lower, &k.to_lowercase())))
        .filter(|(_, d)| *d <= 3)
        .min_by_key(|(_, d)| *d)
        .map(|(k, _)| k.clone())
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

fn value_is_empty(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => true,
        serde_json::Value::String(s) => s.trim().is_empty(),
        _ => false,
    }
}

/// The outcome a rejection resumes its run with.
pub const REJECTED: &str = "rejected";

/// How a resume went, in one sentence for whoever gave it: whether the step
/// the run stood at went on with their `outcome`, as
/// [`WorkflowRuntime::resume_workflow`] said. `mark` wraps the outcome the
/// way the surface it goes to marks a name.
pub fn resumed_message(outcome: Option<&str>, applied: Option<bool>, mark: &str) -> String {
    match (outcome, applied) {
        (Some(outcome), Some(true)) => {
            format!("Workflow resumed with outcome {mark}{outcome}{mark}.")
        }
        (Some(outcome), _) => {
            format!(
                "Workflow resumed, but its step did not go on with outcome {mark}{outcome}{mark}."
            )
        }
        (None, _) => "Workflow resumed.".to_string(),
    }
}

/// Reset a Failed workflow to Paused so it can be resumed. Clears the
/// `_failure_reason` variable so the next step run doesn't see a stale
/// failure marker in its context and contradict the engine's own state.
fn recover_failed_to_paused(instance: &mut WorkflowInstance) {
    instance.status = InstanceStatus::Paused;
    instance.updated_at = Utc::now();
    if let Some(obj) = instance.variables.as_object_mut() {
        obj.remove("_failure_reason");
    }
}

/// The instance variable marking a run held for a spent usage window. Its
/// presence is what [`WorkflowRuntime::resume_usage_capped`] looks for, and
/// the value is when the hold started, so a run held far longer than any
/// real window is visible rather than silently stuck.
const USAGE_CAP_HELD_AT: &str = "_usage_cap_held_at";

/// Hold a run on its current step because the subscription's usage window is
/// spent, rather than failing it. `Paused` with `current_step_id` left where
/// it is means a resume re-runs that step — the same shape as `retry_step`.
/// Any earlier failure marker is dropped: this run has not failed.
fn park_for_usage_cap(instance: &mut WorkflowInstance, reason: &str) {
    instance.status = InstanceStatus::Paused;
    instance.updated_at = Utc::now();
    if let Some(obj) = instance.variables.as_object_mut() {
        obj.remove("_failure_reason");
        obj.insert(
            USAGE_CAP_HELD_AT.to_string(),
            serde_json::Value::String(Utc::now().to_rfc3339()),
        );
        obj.insert(
            "_usage_cap_reason".to_string(),
            serde_json::Value::String(reason.to_string()),
        );
    }
}

/// Whether this instance is held waiting for a usage window to clear.
fn held_for_usage_cap(variables: &serde_json::Value) -> bool {
    variables.get(USAGE_CAP_HELD_AT).is_some()
}

/// Marks a run held because the person it acts as is suspended; the value
/// is when the hold started. [`WorkflowRuntime::resume_suspension_held`]
/// resumes it once whoever the run then acts as is not suspended, so a run
/// a merge moved on to another person goes on as them.
const SUSPENSION_HELD_AT: &str = "_suspension_held_at";

/// Hold a run on its current step while the person it acts as is
/// suspended. The step is left current so resuming runs it again; a person
/// pausing the run themselves leaves no marker, so only this hold is lifted
/// by reactivation.
fn park_for_suspension(instance: &mut WorkflowInstance) {
    instance.status = InstanceStatus::Paused;
    instance.updated_at = Utc::now();
    if let Some(obj) = instance.variables.as_object_mut() {
        obj.remove("_failure_reason");
        obj.insert(
            SUSPENSION_HELD_AT.to_string(),
            serde_json::Value::String(Utc::now().to_rfc3339()),
        );
    }
}

pub(crate) fn held_for_suspension(variables: &serde_json::Value) -> bool {
    variables.get(SUSPENSION_HELD_AT).is_some()
}

fn take_suspension_hold(instance: &mut WorkflowInstance) -> String {
    if let Some(obj) = instance.variables.as_object_mut() {
        obj.remove(SUSPENSION_HELD_AT);
    }
    String::new()
}

fn repark_for_suspension(instance: &mut WorkflowInstance, _: &str) {
    park_for_suspension(instance);
}

/// Drop a usage-cap hold, returning the refusal it was held for.
fn take_usage_cap_hold(instance: &mut WorkflowInstance) -> String {
    let reason = instance
        .variables
        .get("_usage_cap_reason")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    clear_usage_cap_hold(instance);
    reason
}

/// Drop the hold markers once a run is going again, so a later failure that
/// is not a usage cap cannot be mistaken for one still waiting.
fn clear_usage_cap_hold(instance: &mut WorkflowInstance) {
    if let Some(obj) = instance.variables.as_object_mut() {
        obj.remove(USAGE_CAP_HELD_AT);
        obj.remove("_usage_cap_reason");
    }
}

/// Post one status message, retrying once after a short pause when the
/// refusal looks transient. A persistent failure is the caller's to report,
/// with what it cost.
async fn send_status_message(
    surface: &dyn ChannelSurface,
    msg: &OutboundMessage,
) -> Result<String> {
    match surface.send_message(msg).await {
        Ok(sent) => Ok(sent.message_id),
        Err(e) if is_transient_status_error(&e.to_string()) => {
            warn!(
                platform = %surface.platform(),
                channel = %msg.channel,
                error = %e,
                "Workflow status delivery failed transiently; retrying in 1.5s"
            );
            tokio::time::sleep(Duration::from_millis(1500)).await;
            surface.send_message(msg).await.map(|sent| sent.message_id)
        }
        Err(e) => Err(e),
    }
}

/// Classify surface-send errors that are worth retrying once.
///
/// Slack occasionally surfaces `channel_not_found` or `ratelimited` for
/// freshly-created DM channels, during app-permission resyncs, or when the
/// bot-user cache lags behind conversation state. These typically clear
/// within seconds.
fn is_transient_status_error(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("channel_not_found")
        || lower.contains("ratelimited")
        || lower.contains("rate_limited")
        || lower.contains("service_unavailable")
}

/// Strip embedded JSON tool results from an agent response.
///
/// Agents sometimes echo raw JSON from tool calls (e.g. `{"exit_code":0,...}`)
/// in their text output. This function removes lines that are standalone JSON
/// objects so users see clean prose instead of raw tool dumps.
fn strip_tool_json(response: &str) -> String {
    let mut result = String::with_capacity(response.len());
    let mut in_json_block = false;
    let mut brace_depth: i32 = 0;

    for line in response.lines() {
        let trimmed = line.trim();

        if !in_json_block {
            // Detect start of a standalone JSON object
            if trimmed.starts_with('{') && !trimmed.starts_with("{%") {
                // Count braces to handle multi-line JSON
                let opens = trimmed.chars().filter(|&c| c == '{').count() as i32;
                let closes = trimmed.chars().filter(|&c| c == '}').count() as i32;
                brace_depth = opens - closes;
                if brace_depth <= 0 {
                    // Single-line JSON object — skip it
                    continue;
                }
                // Multi-line JSON — start skipping
                in_json_block = true;
                continue;
            }
            // Keep non-JSON lines
            result.push_str(line);
            result.push('\n');
        } else {
            // Inside a multi-line JSON block — count braces
            let opens = trimmed.chars().filter(|&c| c == '{').count() as i32;
            let closes = trimmed.chars().filter(|&c| c == '}').count() as i32;
            brace_depth += opens - closes;
            if brace_depth <= 0 {
                in_json_block = false;
            }
            // Skip the line (it's part of the JSON block)
        }
    }

    // Collapse consecutive blank lines
    let mut prev_blank = false;
    let mut cleaned = String::with_capacity(result.len());
    for line in result.lines() {
        if line.trim().is_empty() {
            if !prev_blank {
                cleaned.push('\n');
            }
            prev_blank = true;
        } else {
            cleaned.push_str(line);
            cleaned.push('\n');
            prev_blank = false;
        }
    }

    cleaned.trim().to_string()
}

impl WorkflowRuntime {
    /// Reject a paused workflow step with an optional reason and feedback.
    ///
    /// Stores the rejection reason in `_rejection_reason` and optional feedback
    /// in `_human_feedback`, then resumes the workflow with outcome `"rejected"`.
    /// The workflow definition's branching logic decides what happens next
    /// (e.g. loop back to the implementation step).
    #[allow(clippy::too_many_arguments)]
    pub async fn reject_workflow_step(
        self: &Arc<Self>,
        instance_id: &str,
        reason: Option<&str>,
        feedback: Option<&str>,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
        by: &Steer,
    ) -> Result<Option<bool>> {
        let rejectable = |instance: &WorkflowInstance| match instance.status {
            InstanceStatus::Paused | InstanceStatus::Failed => Ok(()),
            ref status => Err(eyre!(
                "Cannot reject workflow in status '{status}' — must be paused or failed"
            )),
        };
        let instance = self.load_instance(instance_id)?;
        rejectable(&instance)?;
        self.check_human_gate(&instance, &originating_agent_id)
            .await?;

        self.update_instance(instance_id, |instance| {
            rejectable(instance)?;
            decision::deciding(instance)?;
            if let Some(vars) = instance.variables.as_object_mut() {
                vars.insert(
                    "_rejection_reason".to_string(),
                    serde_json::Value::String(reason.unwrap_or("Rejected by user").to_string()),
                );
                if let Some(fb) = feedback {
                    vars.insert(
                        "_human_feedback".to_string(),
                        serde_json::Value::String(fb.to_string()),
                    );
                }
            }
            instance.updated_at = Utc::now();
            if instance.status == InstanceStatus::Failed {
                recover_failed_to_paused(instance);
            }
            Ok::<_, eyre::Report>(())
        })?;

        info!(
            instance_id = %instance_id,
            reason = ?reason,
            feedback = ?feedback,
            "Workflow step rejected"
        );

        // Resume with "rejected" outcome — the workflow definition's branches
        // determine where execution goes next
        self.resume_workflow(
            instance_id,
            Some(REJECTED.to_string()),
            channel_id,
            thread_id,
            originating_agent_id,
            by,
        )
        .await
    }

    /// Fetch ticket data in-process from Jira or Linear and inject it into the
    /// payload as `ticket_data` so workflow phases can reference
    /// `{{ticket_data}}` directly rather than going through an MCP round-trip.
    ///
    /// Adapter precedence: Jira preferred when configured,
    /// Linear as fallback. Returns `Ok(())` when the payload carries no
    /// `ticket_id` (nothing to fetch) or when one adapter returns a
    /// successful result. Returns `Err` — propagated by `start_workflow` —
    /// when a `ticket_id` is present but no adapter resolves it, so the
    /// workflow refuses to start rather than running on missing data.
    ///
    /// Going via MCP would expose this to Claude Code's ~3-5s stdio
    /// tool-call timeout; ticket APIs routinely take 3-4s, so MCP fetches
    /// flake intermittently. The in-process call has no such timeout.
    async fn prefetch_ticket_data(
        &self,
        payload: &mut serde_json::Value,
        originating_agent_id: Option<&str>,
    ) -> Result<()> {
        let Some(obj) = payload.as_object_mut() else {
            return Ok(());
        };
        let Some(ticket_id) = obj.get("ticket_id").and_then(|v| v.as_str()) else {
            return Ok(());
        };
        let ticket_id = ticket_id.to_string();

        let service = originating_agent_id
            .and_then(|id| self.agent_registry.get().and_then(|reg| reg.get(id)))
            .unwrap_or_else(|| Arc::clone(&self.service));

        for (skill, _, _) in TICKET_PREFETCH_ATTEMPTS {
            Self::ensure_ticket_skill_loaded(&service, skill, &ticket_id);
        }

        let mut last_err: Option<String> = None;
        let mut attempted = false;
        for (skill, tool_name, param) in TICKET_PREFETCH_ATTEMPTS {
            let Some(tool) = service.tools.get(tool_name) else {
                continue;
            };
            attempted = true;
            let mut input_obj = serde_json::Map::new();
            input_obj.insert(
                (*param).to_string(),
                serde_json::Value::String(ticket_id.clone()),
            );
            let err_str = match tool.execute(serde_json::Value::Object(input_obj)).await {
                Ok(result) if result.success => {
                    info!(
                        ticket_id,
                        adapter = skill,
                        "Pre-fetched ticket into payload"
                    );
                    let value = serde_json::to_string_pretty(&result.output)
                        .unwrap_or_else(|_| result.output.to_string());
                    obj.insert("ticket_data".to_string(), serde_json::Value::String(value));
                    return Ok(());
                }
                Ok(result) => result.error.unwrap_or_default(),
                Err(e) => e.to_string(),
            };
            warn!(
                ticket_id,
                adapter = skill,
                error = %err_str,
                "Ticket pre-fetch failed for this adapter"
            );
            last_err = Some(format!("{skill}: {err_str}"));
        }

        let message = format_prefetch_failure(&ticket_id, attempted, last_err.as_deref());
        warn!(ticket_id, %message, "Refusing to start: ticket pre-fetch failed");
        Err(eyre!(message))
    }

    fn ensure_ticket_skill_loaded(service: &Arc<AgentService>, skill: &str, ticket_id: &str) {
        if crate::skill_loader::skill_tools_registered(skill, &service.tools) {
            return;
        }
        let Some(ref vault) = service.vault else {
            return;
        };
        match crate::skill_loader::load_and_track(
            skill,
            Arc::clone(vault),
            service.global_vault.clone(),
            &service.config,
            &service.tools,
            &service.skill_state,
            "workflow-prefetch",
        ) {
            Ok(_) => info!(ticket_id, skill, "Loaded skill for ticket pre-fetch"),
            Err(e) => warn!(
                ticket_id,
                skill,
                error = %e,
                "Failed to load skill for ticket pre-fetch"
            ),
        }
    }

    /// Set a variable on `operator`'s word. Never the engine's own `_…`
    /// ones, which say who the run acts as and where it reports — rewriting
    /// `_run_as` would run the next step as anyone. And only on a run done
    /// for the operator or for nobody: a variable is what the next step is
    /// asked to fetch, with the credentials of the person the run acts as.
    pub fn set_operator_variable(
        &self,
        operator: &TurnPrincipal,
        instance_id: &str,
        key: &str,
        value: &str,
    ) -> Result<()> {
        if key.starts_with('_') {
            return Err(eyre::eyre!(
                "'{key}' is set by the workflow engine and cannot be edited"
            ));
        }
        if !instance_principal(&self.load_instance(instance_id)?).readable_by(operator) {
            return Err(eyre::eyre!(
                "this run acts as someone else, and its variables steer what it fetches for them"
            ));
        }
        self.set_instance_variable(instance_id, key, value)
    }

    pub fn set_instance_variable(&self, instance_id: &str, key: &str, value: &str) -> Result<()> {
        self.update_instance(instance_id, |instance| {
            if let Some(vars) = instance.variables.as_object_mut() {
                vars.insert(
                    key.to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            }
            instance.updated_at = chrono::Utc::now();
            Ok::<_, eyre::Report>(())
        })?;
        info!(instance_id = %instance_id, key = %key, "Set instance variable");
        Ok(())
    }

    /// Anchor a workflow to the Slack message `msg_id`: its thread, and its
    /// channel when it has none yet. The indexed columns follow on save.
    fn anchor_thread(&self, instance_id: &str, channel_id: Option<&str>, msg_id: &str) {
        if let Err(e) = self.update_instance(instance_id, |instance| {
            anchored(instance, channel_id, msg_id);
            Ok::<_, eyre::Report>(())
        }) {
            warn!(instance_id = %instance_id, error = %e, "Failed to anchor the workflow to its thread");
        }
    }

    /// Pause a running workflow. Called from Slack "pause" command.
    /// The workflow will stop after the current step completes.
    pub async fn pause_workflow(&self, instance_id: &str, by: &Steer) -> Result<()> {
        let instance = self.load_instance(instance_id)?;
        if instance.status != InstanceStatus::Running {
            return Err(eyre!(
                "Cannot pause workflow in status '{}'",
                instance.status
            ));
        }
        // Targeted status write valid from any instance — the owning loop
        // notices Paused at its next step boundary.
        self.telemetry_store.update_workflow_instance_status(
            instance_id,
            "paused",
            instance.current_step_id.as_deref(),
        )?;
        let intervention = instance.intervention(InterventionKind::Pause, by.actor().clone());
        crate::decision_records::moved(&instance, intervention);

        // Cancel the running step immediately
        if let Some(token) = self.cancel_tokens.lock().await.remove(instance_id) {
            token.cancel();
            info!(instance_id = %instance_id, "Workflow paused — cancelled running step");
        } else {
            info!(instance_id = %instance_id, "Workflow paused by user");
        }
        Ok(())
    }

    /// Jump to a specific step in a workflow.
    /// Sets status to Paused so that resume_workflow() can restart execution.
    /// Allows recovery of Failed workflows (but not Completed/Exited).
    pub async fn goto_step(&self, instance_id: &str, step_id: &str, by: &Steer) -> Result<()> {
        let movable = |instance: &WorkflowInstance| {
            if matches!(
                instance.status,
                InstanceStatus::Completed | InstanceStatus::Exited | InstanceStatus::Cancelled
            ) {
                return Err(eyre!("Cannot goto step in {} workflow", instance.status));
            }
            if instance.is_queued() {
                return Err(eyre!(
                    "Cannot goto step in a queued workflow — it is holding for a slot and has not run"
                ));
            }
            decision::deciding(instance)
        };
        let instance = self.load_instance(instance_id)?;
        movable(&instance)?;

        // Validate step exists in the definition
        let defs = self.definitions.read().await;
        let def = defs
            .get(&instance.workflow_id)
            .map(|o| &o.def)
            .ok_or_else(|| eyre!("Workflow definition '{}' not found", instance.workflow_id))?;
        if def.get_step(step_id).is_none() {
            let available: Vec<_> = def.steps.iter().map(|s| s.id.as_str()).collect();
            return Err(eyre!(
                "Step '{}' not found. Available steps: {}",
                step_id,
                available.join(", ")
            ));
        }

        drop(defs);
        self.end_noted_job(&instance).await;
        let (instance, intervention) = self.update_instance(instance_id, |instance| {
            movable(instance)?;
            coding_round::clear_note(instance);
            let intervention = instance.intervention(InterventionKind::Goto, by.actor().clone());
            instance.current_step_id = Some(step_id.to_string());
            instance.status = InstanceStatus::Paused;
            instance.updated_at = chrono::Utc::now();
            Ok::<_, eyre::Report>((instance.clone(), intervention))
        })?;
        crate::decision_records::moved(&instance, intervention);

        self.ensure_worktree_alive(instance_id).await;

        info!(instance_id = %instance_id, step = %step_id, "Jumped to step");
        Ok(())
    }

    /// Retry the current step in a failed workflow.
    /// Sets status to Paused so that resume_workflow() can restart execution.
    pub fn retry_step(&self, instance_id: &str, by: &Steer) -> Result<String> {
        let (instance, intervention, step_id) = self.update_instance(instance_id, |instance| {
            decision::deciding(instance)?;
            if instance.status != InstanceStatus::Failed {
                return Err(eyre!(
                    "Can only retry steps in failed workflows (status: {})",
                    instance.status
                ));
            }
            let step_id = instance
                .current_step_id
                .clone()
                .ok_or_else(|| eyre!("No current step to retry"))?;
            let intervention = instance.intervention(InterventionKind::Retry, by.actor().clone());
            instance.status = InstanceStatus::Paused;
            instance.updated_at = chrono::Utc::now();
            Ok((instance.clone(), intervention, step_id))
        })?;
        crate::decision_records::moved(&instance, intervention);
        info!(instance_id = %instance_id, step = %step_id, "Retrying failed step");
        Ok(step_id)
    }

    /// Skip the current step and advance to the next one.
    /// Sets status to Paused so that resume_workflow() can restart execution.
    pub async fn skip_step(self: &Arc<Self>, instance_id: &str, by: &Steer) -> Result<String> {
        let skippable = |instance: &WorkflowInstance| {
            if instance.is_terminal() {
                return Err(eyre!(
                    "Cannot skip step in terminal workflow (status: {})",
                    instance.status
                ));
            }
            if instance.is_queued() {
                return Err(eyre!(
                    "Cannot skip step in a queued workflow — it is holding for a slot and has not run"
                ));
            }
            decision::deciding(instance)?;
            instance
                .current_step_id
                .clone()
                .ok_or_else(|| eyre!("No current step to skip"))
        };
        let instance = self.load_instance(instance_id)?;
        let current = skippable(&instance)?;

        let defs = self.definitions.read().await;
        let def = defs
            .get(&instance.workflow_id)
            .map(|o| &o.def)
            .ok_or_else(|| eyre!("Workflow definition '{}' not found", instance.workflow_id))?;
        let current_idx = def
            .step_index(&current)
            .ok_or_else(|| eyre!("Current step '{}' not in definition", current))?;

        let next_id = def.steps.get(current_idx + 1).map(|step| step.id.clone());
        drop(defs);
        self.end_noted_job(&instance).await;

        let (instance, intervention) = self.update_instance(instance_id, |instance| {
            if skippable(instance)? != current {
                return Err(eyre!(
                    "The run moved off step '{current}' meanwhile; nothing was skipped"
                ));
            }
            coding_round::clear_note(instance);
            let intervention = instance.intervention(InterventionKind::Skip, by.actor().clone());
            match &next_id {
                Some(next_id) => {
                    instance.current_step_id = Some(next_id.clone());
                    instance.status = InstanceStatus::Paused;
                }
                None => {
                    instance.status = InstanceStatus::Completed;
                    instance.current_step_id = None;
                }
            }
            instance.updated_at = chrono::Utc::now();
            Ok((instance.clone(), intervention))
        })?;
        crate::decision_records::moved(&instance, intervention);
        match next_id {
            Some(next_id) => {
                info!(instance_id = %instance_id, skipped = %current, next = %next_id, "Skipped step");
                Ok(next_id)
            }
            None => {
                // Skipping the last step completes the workflow
                info!(instance_id = %instance_id, skipped = %current, "Skipped last step — workflow complete");
                self.promote_queued(&instance.workflow_id).await;
                Ok("(completed)".to_string())
            }
        }
    }
}

/// Anchor `instance` to the Slack message `msg_id`: its thread, and its
/// channel when it records none.
fn anchored(instance: &mut WorkflowInstance, channel_id: Option<&str>, msg_id: &str) {
    if let Some(vars) = instance.variables.as_object_mut() {
        vars.insert("_thread_id".to_string(), serde_json::json!(msg_id));
        if let Some(channel) = channel_id.filter(|_| !vars.contains_key("_channel_id")) {
            vars.insert("_channel_id".to_string(), serde_json::json!(channel));
        }
    }
}

/// A run as it is stored, its indexed channel and thread read from its
/// variables. The lease and the version are the store's own.
fn row_of(instance: &WorkflowInstance) -> WorkflowInstanceRow {
    let var = |key: &str| {
        instance
            .variables
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    WorkflowInstanceRow {
        id: instance.id.clone(),
        workflow_id: instance.workflow_id.clone(),
        status: instance.status.to_string(),
        current_step_id: instance.current_step_id.clone(),
        step_history: serde_json::to_string(&instance.step_history)
            .unwrap_or_else(|_| "[]".to_string()),
        variables: serde_json::to_string(&instance.variables).unwrap_or_else(|_| "{}".to_string()),
        trigger_json: serde_json::to_string(&instance.trigger).unwrap_or_else(|_| "{}".to_string()),
        agent_id: instance.agent_id.clone(),
        created_at: instance.created_at.to_rfc3339(),
        updated_at: instance.updated_at.to_rfc3339(),
        parent_instance_id: instance.parent_instance_id.clone(),
        channel_id: var("_channel_id"),
        thread_id: var("_thread_id"),
        ..Default::default()
    }
}

/// A stored run as the engine reads it. Parts written by a hub that knew
/// more than this one are read as far as they go.
pub fn instance_of(
    row: workforce_common::persistence::WorkflowInstanceRow,
) -> Result<workforce_orchestrator::workflow::instance::WorkflowInstance> {
    let trigger: TriggerInfo = serde_json::from_str(&row.trigger_json).unwrap_or(TriggerInfo {
        source: TriggerSource::Api,
        payload: serde_json::Value::Null,
    });

    // Never read as empty: a run saved again with its history lost is lost
    // for good. History this build cannot read — written by a newer one —
    // leaves the run alone until a build that can picks it up. The error
    // names where, never what: a step history holds what the run's agents
    // wrote.
    let step_history = serde_json::from_str(&row.step_history).map_err(|e| {
        eyre!(
            "{UNREADABLE} workflow instance '{}' has step history this build cannot read \
             ({:?} error at line {}, column {})",
            row.id,
            e.classify(),
            e.line(),
            e.column()
        )
    })?;
    let variables = serde_json::from_str(&row.variables)
        .unwrap_or(serde_json::Value::Object(Default::default()));
    let status: InstanceStatus = row.status.parse().map_err(|e: String| eyre!("{}", e))?;
    let created_at = chrono::DateTime::parse_from_rfc3339(&row.created_at)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let updated_at = chrono::DateTime::parse_from_rfc3339(&row.updated_at)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());

    Ok(
        workforce_orchestrator::workflow::instance::WorkflowInstance {
            id: row.id,
            workflow_id: row.workflow_id,
            status,
            current_step_id: row.current_step_id,
            step_history,
            variables,
            trigger,
            agent_id: row.agent_id,
            parent_instance_id: row.parent_instance_id,
            created_at,
            updated_at,
        },
    )
}

// ── Progress helpers ──────────────────────────────────────────────────────

/// Add a reaction to the step announcement message (best-effort).
async fn add_step_reaction(
    surface: Option<Arc<dyn ChannelSurface>>,
    channel_id: Option<&str>,
    msg_id: Option<&str>,
    emoji: &str,
) {
    if let (Some(surface), Some(cid), Some(mid)) = (surface, channel_id, msg_id) {
        let _ = surface
            .add_reaction(&Reaction {
                channel: cid.to_string(),
                message_id: mid.to_string(),
                emoji: emoji.to_string(),
            })
            .await;
    }
}

/// What a completed step has to say about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportText<'a> {
    /// A declared model-written report; `None` when the model omitted the block.
    Block(Option<&'a str>),
    /// An engine-rendered template, final as it stands.
    Rendered(&'a str),
}

/// What goes out for a step, and how it is treated on the way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepMessage<'a> {
    /// Posted as-is: no step header, no tool-JSON stripping.
    Rendered(&'a str),
    /// Model prose: Full delivery adds the step header; both modes strip tool JSON.
    Prose(&'a str),
}

/// The message a completed step sends to the initiating channel, if any.
///
/// An engine-rendered report is the step's message under either delivery.
/// Under `Reports` delivery a step otherwise speaks only through its declared
/// report — the parsed block when the model emitted one, the full response
/// when it didn't (content over silence) — and an undeclared step is silent.
/// Under `Full` delivery every step posts its response; a paused step prefers
/// its report because the pause card follows immediately.
fn step_outbound_text<'a>(
    delivery: DeliveryMode,
    paused: bool,
    report: Option<ReportText<'a>>,
    response: &'a str,
) -> Option<StepMessage<'a>> {
    match (delivery, report) {
        (_, Some(ReportText::Rendered(text))) => Some(StepMessage::Rendered(text)),
        (DeliveryMode::Reports, Some(ReportText::Block(block))) => {
            Some(StepMessage::Prose(block.unwrap_or(response)))
        }
        (DeliveryMode::Reports, None) => None,
        (DeliveryMode::Full, Some(ReportText::Block(block))) if paused => {
            Some(StepMessage::Prose(block.unwrap_or(response)))
        }
        (DeliveryMode::Full, _) => Some(StepMessage::Prose(response)),
    }
}

/// Consume progress events from a workflow step and show real-time status
/// (emoji reactions on the step message + live status text) in Slack.
async fn consume_step_progress(
    mut progress_rx: tokio::sync::mpsc::Receiver<ProgressEvent>,
    surface: Option<Arc<dyn ChannelSurface>>,
    channel_id: Option<String>,
    thread_id: Option<String>,
    step_msg_id: Option<String>,
) {
    let Some(surface) = surface else {
        warn!("consume_step_progress: no surface available — progress will not be shown in Slack");
        while progress_rx.recv().await.is_some() {}
        return;
    };
    let Some(cid) = channel_id else {
        warn!("consume_step_progress: no channel_id — progress will not be shown in Slack");
        while progress_rx.recv().await.is_some() {}
        return;
    };

    let mut status_msg_id: Option<String> = None;
    let mut current_phase: Option<&str> = None;
    let mut last_update = tokio::time::Instant::now();
    // The semantic status line, stored *already redacted* so the heartbeat and
    // flush re-render secret-safe text. The renderer owns the elapsed counter,
    // so a black-box tool inside a step still ticks instead of freezing.
    let mut base_text: Option<String> = None;
    let mut activity_started = tokio::time::Instant::now();
    let mut dirty = false;
    const THROTTLE: Duration = Duration::from_millis(1500);
    let mut heartbeat = tokio::time::interval(crate::slack_manager::STATUS_HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        let event = tokio::select! {
            maybe = progress_rx.recv() => match maybe {
                Some(ev) => ev,
                None => break,
            },
            _ = heartbeat.tick() => {
                if let Some(base) = base_text.as_ref() {
                    if last_update.elapsed() >= THROTTLE {
                        crate::slack_manager::emit_status(
                            std::slice::from_ref(&surface),
                            &cid,
                            &thread_id,
                            &mut status_msg_id,
                            &crate::slack_manager::render_status(base, activity_started),
                        )
                        .await;
                        last_update = tokio::time::Instant::now();
                        dirty = false;
                    }
                }
                continue;
            }
        };

        let (new_phase, status_text) = match &event {
            ProgressEvent::LlmCall { round } => (
                Some(channels::indicator::THINKING),
                format!("_Thinking (round {})..._", round),
            ),
            ProgressEvent::ToolStart { name, detail, .. } => (
                Some(channels::indicator::TOOL_RUNNING),
                format!(
                    "_Running {}:_ `{}`",
                    crate::slack_manager::friendly_tool_name(name),
                    crate::slack_manager::truncate_detail(detail, 80)
                ),
            ),
            ProgressEvent::CompactStart => (
                Some(channels::indicator::THINKING),
                "_Compacting memory..._".to_string(),
            ),
            _ => continue,
        };

        // Redact once, up front: `base_text` then holds secret-safe text that
        // the heartbeat and flush can re-render without re-redacting.
        let redacted = crate::redact::redact_secrets(&status_text);

        // Reset the elapsed timer only when the line actually changes, so the
        // counter grows monotonically across heartbeats on a quiet tool.
        if base_text.as_deref() != Some(redacted.as_str()) {
            activity_started = tokio::time::Instant::now();
        }

        // Swap emoji reaction on the step announcement message
        if new_phase != current_phase {
            if let Some(old) = current_phase {
                if let Some(ref mid) = step_msg_id {
                    let _ = surface
                        .remove_reaction(&Reaction {
                            channel: cid.clone(),
                            message_id: mid.clone(),
                            emoji: old.to_string(),
                        })
                        .await;
                }
            }
            if let Some(new) = new_phase {
                if let Some(ref mid) = step_msg_id {
                    let _ = surface
                        .add_reaction(&Reaction {
                            channel: cid.clone(),
                            message_id: mid.clone(),
                            emoji: new.to_string(),
                        })
                        .await;
                }
            }
            current_phase = new_phase;
        }

        // Throttled live status message in the thread
        if last_update.elapsed() >= THROTTLE {
            crate::slack_manager::emit_status(
                std::slice::from_ref(&surface),
                &cid,
                &thread_id,
                &mut status_msg_id,
                &crate::slack_manager::render_status(&redacted, activity_started),
            )
            .await;
            last_update = tokio::time::Instant::now();
            dirty = false;
        } else {
            dirty = true;
        }
        base_text = Some(redacted);
    }

    // Flush the final status line if it was throttled — before the completion
    // marker below, so it can't clobber "Step complete ✓".
    if dirty {
        if let Some(base) = base_text.as_ref() {
            crate::slack_manager::emit_status(
                std::slice::from_ref(&surface),
                &cid,
                &thread_id,
                &mut status_msg_id,
                &crate::slack_manager::render_status(base, activity_started),
            )
            .await;
        }
    }

    // Mark the live status message as complete (keep it visible in the thread)
    if let Some(ref msg_id) = status_msg_id {
        let _ = surface
            .edit_message(&cid, msg_id, "_Step complete \u{2713}_")
            .await;
    }

    // Swap the running phase emoji to the done indicator on the step announcement
    if let Some(phase) = current_phase {
        if let Some(ref mid) = step_msg_id {
            let _ = surface
                .remove_reaction(&Reaction {
                    channel: cid.clone(),
                    message_id: mid.clone(),
                    emoji: phase.to_string(),
                })
                .await;
            let _ = surface
                .add_reaction(&Reaction {
                    channel: cid.clone(),
                    message_id: mid.clone(),
                    emoji: channels::indicator::DONE.to_string(),
                })
                .await;
        }
    }
}

/// Wall clock for a `claude-code` step whose definition sets none.
const DEFAULT_STEP_TIMEOUT_SECS: u64 = 30 * 60;

/// Claude Code's own file-and-shell tools, what a `claude-code` step may use
/// when its definition does not say.
const DEFAULT_STEP_TOOLS: &[&str] = &["Read", "Edit", "Write", "Bash", "Glob", "Grep"];

/// The Claude Code tools a step may use: its own list, else the default set.
fn step_allowed_tools(
    exec: &workforce_orchestrator::workflow::engine::StepExecution,
) -> Vec<String> {
    if exec.allowed_tools.is_empty() {
        DEFAULT_STEP_TOOLS.iter().map(|s| s.to_string()).collect()
    } else {
        exec.allowed_tools.clone()
    }
}

/// The project a workflow step's remote workspace is prepared from: the repo a
/// prior step named as its target, first, then every other repo named for the
/// workspace (whitespace or comma separated), each checked out under its name;
/// no repos until a step names a target.
fn step_project(
    agent_id: &str,
    target_repo: Option<&str>,
    workspace_repos: Option<&str>,
) -> Result<workforce_projects::Project> {
    let mut project = workforce_projects::Project::new(agent_id, step_project_name(target_repo));
    let Some(target_repo) = target_repo else {
        return Ok(project);
    };
    let workspace = workspace_repos
        .unwrap_or_default()
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|entry| !entry.is_empty())
        .map(|entry| github_repo("workspace_repos entry", entry));
    project.repos = std::iter::once(github_repo("target_repo", target_repo))
        .chain(workspace)
        .try_fold(Vec::new(), with_repo)?;
    Ok(project)
}

/// `repos` with `repo` added unless it already holds it, whatever the case;
/// another owner's repo of the same name would share its checkout.
fn with_repo(
    mut repos: Vec<workforce_projects::ProjectRepo>,
    repo: Result<workforce_projects::ProjectRepo>,
) -> Result<Vec<workforce_projects::ProjectRepo>> {
    let repo = repo?;
    match repos
        .iter()
        .find(|kept| kept.alias.eq_ignore_ascii_case(&repo.alias))
    {
        None => repos.push(repo),
        Some(kept) if kept.source.canonical_owner() == repo.source.canonical_owner() => {}
        Some(kept) => bail!(
            "workspace_repos names {} and {}, which would share the checkout `{}`",
            kept.source.canonical_slug(),
            repo.source.canonical_slug(),
            repo.alias
        ),
    }
    Ok(repos)
}

/// What a workflow's remote scope is keyed by: its target repo alone, so every
/// step finds the same scope however the workspace around the target is named.
fn step_project_name(target_repo: Option<&str>) -> String {
    target_repo.map_or_else(|| "workflow".to_string(), |repo| format!("workflow:{repo}"))
}

/// The GitHub repo `slug` names as `owner/repo`, checked out under its name;
/// `field` says where the slug came from when it is not one. Each part keeps
/// to GitHub's own name characters, since the worker makes the name a dataset
/// and a path and puts the slug in a clone command.
fn github_repo(field: &str, slug: &str) -> Result<workforce_projects::ProjectRepo> {
    let github_name = |part: &str| {
        !matches!(part, "" | "." | "..")
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    let (owner, repo) = slug
        .split_once('/')
        .filter(|(owner, repo)| github_name(owner) && github_name(repo))
        .ok_or_else(|| eyre!("{field} {slug:?} is not owner/repo"))?;
    Ok(workforce_projects::ProjectRepo {
        alias: repo.to_string(),
        source: workforce_projects::RepoSource::Github {
            owner: owner.to_string(),
            repo: repo.to_string(),
        },
        default_branch: None,
        setup: None,
    })
}

#[async_trait::async_trait]
impl crate::pipeline::HubReads for WorkflowRuntime {
    async fn runs(
        &self,
        filter: &workforce_common::persistence::InstanceFilter,
        agent: Option<&str>,
    ) -> Result<Vec<crate::pipeline::runs::Run>> {
        use workforce_common::persistence::{InstanceFilter, RunsAs};
        let runs_as = match agent {
            Some(agent) => Some(RunsAs {
                agent: agent.to_string(),
                declaring: self
                    .definitions
                    .read()
                    .await
                    .iter()
                    .filter(|(_, owned)| owned.def.workflow.agent.as_deref() == Some(agent))
                    .map(|(key, _)| key.clone())
                    .collect(),
            }),
            None => None,
        };
        let rows = self
            .telemetry_store
            .list_workflow_instances_filtered(&InstanceFilter {
                runs_as,
                ..filter.clone()
            })?;
        let declared = self
            .declared_agents(rows.iter().map(|row| row.workflow_id.as_str()))
            .await;
        Ok(crate::pipeline::runs::runs_as_declared(rows, &declared).collect())
    }

    async fn facts(
        &self,
        agent: Option<&str>,
        sources: &[String],
        caller: &crate::hub_core::pipelines::Caller,
    ) -> Result<crate::pipeline::facts::Facts> {
        use crate::pipeline::facts;
        let schedules = match self.service.scheduler.as_deref() {
            Some(scheduler) => {
                futures::future::join_all(scheduler.list_jobs().await.into_iter().map(
                    |job| async move {
                        let last = scheduler.recent_outcomes(&job.id, facts::LAST_RUNS).await;
                        (job, last)
                    },
                ))
                .await
            }
            None => Vec::new(),
        };
        let skills = crate::wasm_skill_tools::global_skill_store()
            .map(|store| {
                crate::skill_access::SkillCaller::new(
                    caller.identity.clone(),
                    caller.admin || caller.system,
                    TeamReach::for_service(&self.service, caller.identity.clone()),
                )
                .visible(&store)
                .into_iter()
                .map(|(skill, _)| skill)
                .collect()
            })
            .unwrap_or_default();
        let vault = crate::wasm_skill_tools::SkillLockerVault::compose(
            self.service.global_vault.clone(),
            self.service.vault.clone(),
        );
        let definitions = self.definitions.read().await;
        Ok(facts::facts(
            facts::Inputs {
                agent,
                sources,
                workflows: definitions
                    .iter()
                    .map(|(key, owned)| facts::Workflow {
                        key,
                        def: &owned.def,
                        content_hash: owned.content_hash.as_deref(),
                    })
                    .collect(),
                skills,
                schedules,
                workers: crate::coding_sandbox::worker_registry().snapshot(),
                build: crate::build_info::as_json(),
            },
            |skill| {
                vault
                    .as_deref()
                    .and_then(|vault| facts::grant_of(vault, skill, caller.identity.as_deref()))
            },
        ))
    }
}

/// The workflow runtime's definition store, and the watcher that
/// hot-reloads its definitions.
pub(crate) async fn wire(
    service: &Arc<crate::serve::AgentService>,
    config: &workforce_common::settings::WorkforceConfig,
    workflow_runtime: Arc<WorkflowRuntime>,
) -> Arc<dyn crate::hosts::WorkflowHost> {
    let workflows_dir = workflow_runtime.workflows_dir.clone();
    // Definitions move to Postgres when available (DW-550); the
    // directory remains the fallback. Load from DB after attaching.
    let wf_def_store = shared_workflow_store(config).await;
    if let Some(ref store) = wf_def_store {
        workflow_runtime.set_store(Arc::clone(store));
        if let Err(e) = workflow_runtime.reload_definitions().await {
            warn!(error = ?e, "Failed to load workflow definitions from DB");
        }
    }
    if let Some(store) = wf_def_store.clone() {
        // DB poll for out-of-band definition edits (in-process saves
        // already reload synchronously). Replaces the TOML fs_watcher.
        let wf_runtime = Arc::clone(&workflow_runtime);
        tokio::spawn(async move {
            let mut last = store.change_marker("workflow").await.ok();
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            tick.tick().await; // consume the immediate first tick
            loop {
                tick.tick().await;
                match store.change_marker("workflow").await {
                    Ok(cur) if Some(cur) != last => {
                        last = Some(cur);
                        match wf_runtime.reload_definitions().await {
                            Ok(count) => {
                                info!(count, "Reloaded workflow definitions from DB")
                            }
                            Err(e) => {
                                tracing::warn!("DB workflow reload failed: {e}")
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("workflow def poll failed: {e}"),
                }
            }
        });
    } else {
        // FsWatcher: hot-reload workflow definitions when TOML files change
        let wf_runtime = Arc::downgrade(&workflow_runtime);
        service
            .watches
            .lock()
            .extend(crate::fs_watcher::spawn_on_change(
                "workflow definitions",
                crate::fs_watcher::FsWatcherConfig {
                    paths: vec![workflows_dir.to_string_lossy().to_string()],
                    patterns: vec!["*.toml".to_string()],
                    debounce_ms: 1000,
                    recursive: true,
                },
                move || {
                    wf_runtime.upgrade().map(|runtime| async move {
                        match runtime.reload_definitions().await {
                            Ok(count) => {
                                info!(count, "Hot-reloaded workflow definitions")
                            }
                            Err(e) => {
                                tracing::warn!("Failed to hot-reload workflow definitions: {}", e)
                            }
                        }
                    })
                },
            ));
    }

    workflow_runtime as Arc<dyn crate::hosts::WorkflowHost>
}

#[async_trait::async_trait]
impl crate::hosts::DefinitionSurface for WorkflowRuntime {
    fn def_store(&self) -> Option<&Arc<workforce_orchestrator::workflow_store::WorkflowStore>> {
        WorkflowRuntime::def_store(self)
    }

    async fn definition_ownership(
        &self,
        bare_id: &str,
        owner: Option<&str>,
    ) -> Option<(Option<String>, Visibility)> {
        WorkflowRuntime::definition_ownership(self, bare_id, owner).await
    }

    async fn owners(&self) -> Vec<(String, Option<String>)> {
        self.list_definitions()
            .await
            .into_iter()
            .map(|d| (d.id, d.owner))
            .collect()
    }

    async fn reload(&self) -> Result<usize> {
        self.reload_definitions().await
    }
}

#[async_trait::async_trait]
impl crate::hosts::WorkflowHost for WorkflowRuntime {
    fn set_store(&self, store: Arc<workforce_orchestrator::workflow_store::WorkflowStore>) {
        WorkflowRuntime::set_store(self, store)
    }

    fn set_mcp_exposed_tools(&self, patterns: Vec<String>) {
        WorkflowRuntime::set_mcp_exposed_tools(self, patterns)
    }

    fn set_agent_registry(&self, registry: Arc<dyn crate::hosts::RegistryView>) {
        WorkflowRuntime::set_agent_registry(self, registry)
    }

    fn set_agent_surfaces(
        &self,
        agent_bindings: &HashMap<String, String>,
        workspace_surfaces: &HashMap<String, Arc<dyn ChannelSurface>>,
    ) {
        WorkflowRuntime::set_agent_surfaces(self, agent_bindings, workspace_surfaces)
    }

    fn set_shutdown_token(&self, token: CancellationToken) {
        WorkflowRuntime::set_shutdown_token(self, token)
    }

    fn retarget_desk(
        &self,
        owner: &str,
        retarget: &(dyn for<'t> Fn(&'t DeskTarget) -> Option<DeskTarget> + Send + Sync),
    ) -> Vec<String> {
        WorkflowRuntime::retarget_desk(self, owner, retarget)
    }

    async fn reload_definitions(&self) -> eyre::Result<usize> {
        WorkflowRuntime::reload_definitions(self).await
    }

    async fn sync_schedule_triggers(&self) -> eyre::Result<usize> {
        WorkflowRuntime::sync_schedule_triggers(self).await
    }

    fn can_schedule(&self) -> bool {
        WorkflowRuntime::can_schedule(self)
    }

    async fn recover_running_instances(self: Arc<Self>) {
        WorkflowRuntime::recover_running_instances(&self).await
    }

    async fn start_maintenance_loop(
        self: Arc<Self>,
        interval_secs: u64,
        drain_token: CancellationToken,
    ) {
        WorkflowRuntime::start_maintenance_loop(&self, interval_secs, drain_token).await
    }

    async fn resume_suspension_held(self: Arc<Self>) {
        WorkflowRuntime::resume_suspension_held(&self).await
    }

    async fn list_definitions(&self) -> Vec<WorkflowSummary> {
        WorkflowRuntime::list_definitions(self).await
    }

    async fn list_definitions_visible(&self, listing: &DefListing) -> Vec<WorkflowSummary> {
        WorkflowRuntime::list_definitions_visible(self, listing).await
    }

    async fn get_definition(
        &self,
        id: &str,
    ) -> Option<workforce_orchestrator::workflow::definition::WorkflowDefinition> {
        WorkflowRuntime::get_definition(self, id).await
    }

    async fn all_definitions_keyed(
        &self,
    ) -> Vec<(
        String,
        workforce_orchestrator::workflow::definition::WorkflowDefinition,
    )> {
        WorkflowRuntime::all_definitions_keyed(self).await
    }

    async fn readable_definition(
        &self,
        key: &str,
        reader: &crate::def_write_auth::DefWriter,
    ) -> Option<workforce_orchestrator::workflow::definition::WorkflowDefinition> {
        WorkflowRuntime::readable_definition(self, key, reader).await
    }

    async fn declared_agents(&self, keys: &[&str]) -> HashMap<String, String> {
        WorkflowRuntime::declared_agents(self, keys.iter().copied()).await
    }

    fn get_definition_source(&self, id: &str, owner: Option<&str>) -> eyre::Result<String> {
        WorkflowRuntime::get_definition_source(self, id, owner)
    }

    async fn save_definition_source(
        &self,
        id: &str,
        toml_content: &str,
        owner: Option<&str>,
        visibility: Visibility,
        run_as: workforce_orchestrator::workflow_store::RunAsWrite,
        edit: workforce_orchestrator::workflow_store::EditContext<'_>,
    ) -> eyre::Result<Vec<String>> {
        WorkflowRuntime::save_definition_source(
            self,
            id,
            toml_content,
            owner,
            visibility,
            run_as,
            edit,
        )
        .await
    }

    async fn delete_definition(
        &self,
        id: &str,
        owner: Option<&str>,
        edit: workforce_orchestrator::workflow_store::EditContext<'_>,
    ) -> eyre::Result<()> {
        WorkflowRuntime::delete_definition(self, id, owner, edit).await
    }

    fn export_all_sources(&self) -> eyre::Result<HashMap<String, String>> {
        WorkflowRuntime::export_all_sources(self)
    }

    async fn list_revisions(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> eyre::Result<Vec<workforce_orchestrator::workflow_store::DefRevision>> {
        WorkflowRuntime::list_revisions(self, id, owner).await
    }

    async fn get_revision(
        &self,
        id: &str,
        owner: Option<&str>,
        rev: i32,
    ) -> eyre::Result<Option<workforce_orchestrator::workflow_store::DefRevision>> {
        WorkflowRuntime::get_revision(self, id, owner, rev).await
    }

    async fn definition_run_as(&self, bare_id: &str, owner: Option<&str>) -> Option<String> {
        WorkflowRuntime::definition_run_as(self, bare_id, owner).await
    }

    async fn automated_run_block(
        &self,
        key: &str,
        run_identity: Option<&str>,
    ) -> Option<AutomatedRunBlock> {
        WorkflowRuntime::automated_run_block(self, key, run_identity).await
    }

    async fn annotate_automation(&self, summaries: Vec<WorkflowSummary>) -> Vec<WorkflowSummary> {
        WorkflowRuntime::annotate_automation(self, summaries).await
    }

    async fn summaries_json(&self, summaries: &[WorkflowSummary]) -> Vec<serde_json::Value> {
        WorkflowRuntime::summaries_json(self, summaries).await
    }

    async fn with_trigger_urls(
        &self,
        def_key: &str,
        entry: serde_json::Value,
    ) -> serde_json::Value {
        WorkflowRuntime::with_trigger_urls(self, def_key, entry).await
    }

    async fn trigger_urls(&self, def_key: &str) -> Vec<TriggerUrls> {
        WorkflowRuntime::trigger_urls(self, def_key).await
    }

    async fn resolve_http_trigger(
        &self,
        public_path: &str,
    ) -> Option<(
        String,
        workforce_orchestrator::workflow::definition::TriggerDefinition,
    )> {
        WorkflowRuntime::resolve_http_trigger(self, public_path).await
    }

    fn owner_run_quota_exceeded(&self, owner: Option<&str>) -> bool {
        WorkflowRuntime::owner_run_quota_exceeded(self, owner)
    }

    async fn start_workflow(
        self: Arc<Self>,
        workflow_id: &str,
        payload: serde_json::Value,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
        parent_instance_id: Option<String>,
    ) -> eyre::Result<String> {
        WorkflowRuntime::start_workflow(
            &self,
            workflow_id,
            payload,
            channel_id,
            thread_id,
            originating_agent_id,
            parent_instance_id,
        )
        .await
    }

    async fn start_workflow_from_trigger(
        self: Arc<Self>,
        workflow_id: &str,
        trigger: workforce_orchestrator::workflow::instance::TriggerInfo,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
        run_as: Option<String>,
    ) -> eyre::Result<String> {
        WorkflowRuntime::start_workflow_from_trigger(
            &self,
            workflow_id,
            trigger,
            channel_id,
            thread_id,
            originating_agent_id,
            run_as,
        )
        .await
    }

    async fn start_workflow_streaming(
        self: Arc<Self>,
        workflow_id: &str,
        trigger: workforce_orchestrator::workflow::instance::TriggerInfo,
        run_as: Option<String>,
    ) -> eyre::Result<(
        String,
        tokio::sync::broadcast::Receiver<WorkflowStreamEvent>,
    )> {
        WorkflowRuntime::start_workflow_streaming(&self, workflow_id, trigger, run_as).await
    }

    async fn resume_workflow(
        self: Arc<Self>,
        instance_id: &str,
        outcome: Option<String>,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
        by: &crate::run_steering::Steer,
    ) -> eyre::Result<Option<bool>> {
        WorkflowRuntime::resume_workflow(
            &self,
            instance_id,
            outcome,
            channel_id,
            thread_id,
            originating_agent_id,
            by,
        )
        .await
    }

    async fn reject_workflow_step(
        self: Arc<Self>,
        instance_id: &str,
        reason: Option<&str>,
        feedback: Option<&str>,
        channel_id: Option<String>,
        thread_id: Option<String>,
        originating_agent_id: Option<String>,
        by: &crate::run_steering::Steer,
    ) -> eyre::Result<Option<bool>> {
        WorkflowRuntime::reject_workflow_step(
            &self,
            instance_id,
            reason,
            feedback,
            channel_id,
            thread_id,
            originating_agent_id,
            by,
        )
        .await
    }

    async fn abort_workflow(
        self: Arc<Self>,
        instance_id: &str,
        by: &crate::run_steering::Steer,
    ) -> eyre::Result<()> {
        WorkflowRuntime::abort_workflow(&self, instance_id, by).await
    }

    async fn pause_workflow(
        &self,
        instance_id: &str,
        by: &crate::run_steering::Steer,
    ) -> eyre::Result<()> {
        WorkflowRuntime::pause_workflow(self, instance_id, by).await
    }

    async fn goto_step(
        &self,
        instance_id: &str,
        step_id: &str,
        by: &crate::run_steering::Steer,
    ) -> eyre::Result<()> {
        WorkflowRuntime::goto_step(self, instance_id, step_id, by).await
    }

    fn retry_step(
        &self,
        instance_id: &str,
        by: &crate::run_steering::Steer,
    ) -> eyre::Result<String> {
        WorkflowRuntime::retry_step(self, instance_id, by)
    }

    async fn skip_step(
        self: Arc<Self>,
        instance_id: &str,
        by: &crate::run_steering::Steer,
    ) -> eyre::Result<String> {
        WorkflowRuntime::skip_step(&self, instance_id, by).await
    }

    async fn authorize_steer(
        &self,
        instance_id: &str,
        steerer: &crate::run_steering::Steerer,
        action: crate::run_steering::SteerAction,
    ) -> std::result::Result<crate::run_steering::Steer, crate::run_steering::SteerRefused> {
        WorkflowRuntime::authorize_steer(self, instance_id, steerer, action).await
    }

    async fn decide(
        self: Arc<Self>,
        instance_id: &str,
        ask: decision::DecisionAsk,
        steerer: &crate::run_steering::Steerer,
    ) -> std::result::Result<decision::DecisionMade, decision::DecisionRefused> {
        WorkflowRuntime::decide(&self, instance_id, ask, steerer).await
    }

    async fn resume_as(
        self: Arc<Self>,
        id: &str,
        ask: decision::ResumeAsk,
        steerer: &crate::run_steering::Steerer,
    ) -> std::result::Result<decision::Resumed, decision::ResumeRefused> {
        WorkflowRuntime::resume_as(&self, id, ask, steerer).await
    }

    fn decision_view(
        &self,
        row: &WorkflowInstanceRow,
        viewer: Option<&str>,
    ) -> Option<serde_json::Value> {
        WorkflowRuntime::decision_view(self, row, viewer)
    }

    fn decision_reply(
        &self,
        channel: &str,
        thread_ts: &str,
        text: &str,
    ) -> Option<(String, uuid::Uuid, String)> {
        WorkflowRuntime::decision_reply(self, channel, thread_ts, text)
    }

    fn decision_reminder(&self, row: &WorkflowInstanceRow) -> Option<String> {
        WorkflowRuntime::decision_reminder(self, row)
    }

    async fn repair_card(
        &self,
        instance_id: &str,
        gate: uuid::Uuid,
        channel: &str,
        ts: &str,
        echoed: Option<serde_json::Value>,
        response_url: &str,
    ) {
        WorkflowRuntime::repair_card(self, instance_id, gate, channel, ts, echoed, response_url)
            .await
    }

    fn get_instance_status(&self, instance_id: &str) -> eyre::Result<Option<WorkflowInstanceRow>> {
        WorkflowRuntime::get_instance_status(self, instance_id)
    }

    fn list_instances(
        &self,
        status: Option<&str>,
        workflow_id: Option<&str>,
    ) -> eyre::Result<Vec<WorkflowInstanceRow>> {
        WorkflowRuntime::list_instances(self, status, workflow_id)
    }

    async fn blocking_instance(&self, workflow_id: &str) -> eyre::Result<Option<String>> {
        WorkflowRuntime::blocking_instance(self, workflow_id).await
    }

    fn find_instance_by_thread(
        &self,
        channel_id: &str,
        thread_id: &str,
        statuses: &[&str],
    ) -> eyre::Result<Option<WorkflowInstanceRow>> {
        WorkflowRuntime::find_instance_by_thread(self, channel_id, thread_id, statuses)
    }

    async fn readable_runs(
        &self,
        rows: Vec<WorkflowInstanceRow>,
        reader: &crate::def_write_auth::DefWriter,
    ) -> Vec<WorkflowInstanceRow> {
        WorkflowRuntime::readable_runs(self, rows, reader).await
    }

    async fn readable_run(
        &self,
        instance_id: &str,
        reader: &crate::def_write_auth::DefWriter,
    ) -> eyre::Result<Option<WorkflowInstanceRow>> {
        WorkflowRuntime::readable_run(self, instance_id, reader).await
    }

    fn set_operator_variable(
        &self,
        operator: &TurnPrincipal,
        instance_id: &str,
        key: &str,
        value: &str,
    ) -> eyre::Result<()> {
        WorkflowRuntime::set_operator_variable(self, operator, instance_id, key, value)
    }

    fn set_instance_variable(&self, instance_id: &str, key: &str, value: &str) -> eyre::Result<()> {
        WorkflowRuntime::set_instance_variable(self, instance_id, key, value)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_tool_steps_answer_survives_an_interrupted_loop_and_is_used_once() {
        use super::{note_reported_tool_result, take_reported_tool_result, TOOL_REPORTED_KEY};
        use workforce_orchestrator::workflow::instance::{
            TriggerInfo, TriggerSource, WorkflowInstance,
        };
        // The tool answered, the loop died before the step was recorded, the
        // instance was saved and reloaded: the answer is there for the same
        // step, once, and nothing for another step.
        let trigger = TriggerInfo {
            source: TriggerSource::Api,
            payload: serde_json::json!({}),
        };
        let mut instance = WorkflowInstance::new("order-review", trigger, "pick", None);
        let answer = "OUTPUTS:\nticket_id: ORD-2779\nEND_OUTPUTS";
        note_reported_tool_result(&mut instance, "pick", answer);
        let saved = serde_json::to_string(&instance.variables).unwrap();
        instance.variables = serde_json::from_str(&saved).unwrap();

        assert_eq!(
            take_reported_tool_result(&mut instance, "ci"),
            None,
            "another step's turn"
        );
        assert_eq!(
            take_reported_tool_result(&mut instance, "pick").as_deref(),
            Some(answer)
        );
        assert_eq!(
            take_reported_tool_result(&mut instance, "pick"),
            None,
            "taken once"
        );
        assert!(instance.variables.get(TOOL_REPORTED_KEY).is_none());
    }

    /// Full delivery posts every response (a model's report only on pause);
    /// Reports delivery posts only declared reports, falling back to the
    /// response when the model skipped its REPORT block, and is silent
    /// otherwise. An engine-rendered report is the step's message everywhere.
    #[test]
    fn step_outbound_text_honors_the_delivery_mode() {
        use super::{step_outbound_text, ReportText, StepMessage};
        use workforce_orchestrator::workflow::definition::DeliveryMode::{Full, Reports};

        let block = Some(ReportText::Block(Some("r")));
        let omitted = Some(ReportText::Block(None));
        assert_eq!(
            step_outbound_text(Full, false, block, "resp"),
            Some(StepMessage::Prose("resp"))
        );
        assert_eq!(
            step_outbound_text(Full, true, block, "resp"),
            Some(StepMessage::Prose("r"))
        );
        assert_eq!(
            step_outbound_text(Full, true, omitted, "resp"),
            Some(StepMessage::Prose("resp"))
        );
        assert_eq!(
            step_outbound_text(Reports, false, block, "resp"),
            Some(StepMessage::Prose("r"))
        );
        assert_eq!(
            step_outbound_text(Reports, false, omitted, "resp"),
            Some(StepMessage::Prose("resp")),
            "a declared report the model failed to emit must still deliver content"
        );
        assert_eq!(
            step_outbound_text(Reports, false, None, "resp"),
            None,
            "an undeclared step must stay silent under reports delivery"
        );
        assert_eq!(
            step_outbound_text(Reports, true, None, "resp"),
            None,
            "pausing does not exempt an undeclared step from reports delivery"
        );
        for (delivery, paused) in [
            (Full, false),
            (Full, true),
            (Reports, false),
            (Reports, true),
        ] {
            assert_eq!(
                step_outbound_text(delivery, paused, Some(ReportText::Rendered("t")), "resp"),
                Some(StepMessage::Rendered("t")),
                "a rendered report is the step's message in every mode"
            );
        }
    }

    /// The default agent is built twice — once to mint the service the scheduler
    /// references, then again with the scheduler tool. Adopting the first pass's
    /// runtime binds the hub to a service with no scheduler, and every cron
    /// silently stops registering.
    #[tokio::test]
    async fn the_live_service_runtime_can_arm_schedules() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = workforce_common::settings::WorkforceConfig::default();
        cfg.hub.data_dir = tmp.path().to_string_lossy().to_string();
        cfg.memory.db_path = std::path::Path::new(&cfg.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        cfg.llm.api_key = Some("test-key".into());
        std::mem::forget(tmp);
        let (service, _scheduler) = crate::serve::AgentService::from_config_for_hub(
            cfg.clone(),
            None,
            None,
            false,
            crate::subscription::Subscriptions::boot(&cfg, None),
        )
        .await
        .expect("test service");
        assert!(
            service
                .workflow_runtime()
                .expect("live service must expose a runtime")
                .can_schedule(),
            "the live runtime must be bound to a service that owns a scheduler"
        );
    }

    /// `start_workflow` — not the caller's payload — owns the delivery keys:
    /// the trusted `channel_id`/`thread_id` arguments seed `_channel_id`/
    /// `_thread_id` into variables (Slack thread matching and the thread-index
    /// columns depend on them), while a payload-smuggled copy is stripped at
    /// construction and never survives.
    #[tokio::test]
    async fn start_workflow_seeds_delivery_context_from_trusted_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = workforce_common::settings::WorkforceConfig::default();
        cfg.hub.data_dir = tmp.path().to_string_lossy().to_string();
        cfg.memory.db_path = std::path::Path::new(&cfg.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        cfg.llm.api_key = Some("test-key".into());
        let (service, _scheduler) = crate::serve::AgentService::from_config_for_hub(
            cfg.clone(),
            None,
            None,
            false,
            crate::subscription::Subscriptions::boot(&cfg, None),
        )
        .await
        .expect("test service");

        let workflows_dir = tmp.path().join("wf-defs");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        // A tool step naming an unknown tool fails fast and offline, so the
        // spawned execute_loop can't hang the test on a model call.
        std::fs::write(
            workflows_dir.join("seed.toml"),
            "[workflow]\nid = \"wf-seed\"\nname = \"Seed\"\n\
             \n[workflow.context]\nrequest = { required = false }\n\
             \n[[step]]\nid = \"s1\"\nname = \"s1\"\ntask = \"{}\"\n\
             tool = \"nope.nope\"\nexit = true\n",
        )
        .unwrap();

        let runtime = Arc::new(WorkflowRuntime::new(
            Arc::clone(&service),
            Arc::clone(&service.telemetry_store),
            workflows_dir,
        ));
        runtime
            .reload_definitions()
            .await
            .expect("definitions load");

        let id = runtime
            .start_workflow(
                "wf-seed",
                serde_json::json!({
                    "request": "r",
                    "_channel_id": "C_SPOOF",
                    "_thread_id": "9.9",
                }),
                Some("C_REAL".into()),
                Some("168.42".into()),
                None,
                None,
            )
            .await
            .expect("start");

        let instance = runtime.load_instance(&id).expect("instance saved");
        assert_eq!(instance.variables["_channel_id"], "C_REAL");
        assert_eq!(instance.variables["_thread_id"], "168.42");
        assert_eq!(instance.variables["request"], "r");
    }

    #[test]
    fn a_run_reports_to_its_slack_thread_else_its_desk_conversation_else_nowhere() {
        use workforce_common::channels::desk::DeskPlace;
        let desk = || {
            Some(DeskScope {
                target: DeskTarget::of_turn(&DeskPlace::Dm, "desk", None, false),
                attended: false,
                relayed: false,
                owner: "person-1".into(),
            })
        };
        assert_eq!(
            destination_of(Some("C1"), Some("1.0"), desk()),
            Destination::Slack {
                channel: "C1",
                thread: Some("1.0")
            }
        );
        assert_eq!(
            destination_of(None, Some("1.0"), desk()),
            Destination::Desk(desk().unwrap())
        );
        assert_eq!(destination_of(None, None, None), Destination::Dark);
    }

    /// A run started from a Desk thread remembers it, and what it says when
    /// it ends goes there, as a workflow message, with no Slack involved.
    #[tokio::test]
    async fn a_run_started_on_desk_reports_its_end_there() {
        use workforce_common::channels::desk::DeskPlace;
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = workforce_common::settings::WorkforceConfig::default();
        cfg.hub.data_dir = tmp.path().to_string_lossy().to_string();
        cfg.memory.db_path = tmp.path().join("memory.db").to_string_lossy().into_owned();
        cfg.llm.api_key = Some("test-key".into());
        let (service, _scheduler) = crate::serve::AgentService::from_config_for_hub(
            cfg.clone(),
            None,
            None,
            false,
            crate::subscription::Subscriptions::boot(&cfg, None),
        )
        .await
        .expect("test service");
        let slack = crate::serve::testing::SentToSlack::on(&service).await;
        let workflows_dir = tmp.path().join("wf-defs");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        std::fs::write(
            workflows_dir.join("seed.toml"),
            "[workflow]\nid = \"wf-desk\"\nname = \"Desk run\"\n\
             \n[[step]]\nid = \"s1\"\nname = \"s1\"\ntask = \"{}\"\n\
             tool = \"nope.nope\"\nexit = true\n",
        )
        .unwrap();
        let runtime = Arc::new(WorkflowRuntime::new(
            Arc::clone(&service),
            Arc::clone(&service.telemetry_store),
            workflows_dir,
        ));
        runtime
            .reload_definitions()
            .await
            .expect("definitions load");
        let room = uuid::Uuid::from_u128(41);
        let target = DeskTarget::of_turn(
            &DeskPlace::Room {
                room,
                name: "Planning".into(),
                thread: Some(uuid::Uuid::from_u128(42)),
            },
            &format!("desk:{room}"),
            None,
            false,
        );
        const PERSON: &str = "4f1d2c3b-5a69-4788-9a0b-1c2d3e4f5a6b";
        let id = workforce_agent::runner::CURRENT_USER_ID
            .scope(
                PERSON.to_string(),
                workforce_agent::runner::with_desk(
                    Some(DeskScope {
                        target: target.clone(),
                        attended: true,
                        relayed: false,
                        owner: "person-1".into(),
                    }),
                    Arc::clone(&runtime).start_workflow(
                        "wf-desk",
                        serde_json::json!({}),
                        None,
                        None,
                        Some(service.agent_id.to_string()),
                        None,
                    ),
                ),
            )
            .await
            .expect("start");
        assert_eq!(
            desk_of(&runtime.load_instance(&id).unwrap()),
            Some(target.clone())
        );

        let said = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let workflow_said = service
                    .sessions
                    .find(&target.channel, PERSON)
                    .ok()
                    .flatten()
                    .and_then(|session| {
                        session.messages.into_iter().find(|m| {
                            m.desk.as_ref().and_then(|mark| mark.delivery)
                                == Some(DeliveryKind::Workflow)
                        })
                    });
                if let Some(said) = workflow_said {
                    return said;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the run reports to Desk");
        assert_eq!(
            said.desk.and_then(|mark| mark.thread),
            Some(uuid::Uuid::from_u128(42))
        );
        assert!(slack.sent().is_empty(), "{:?}", slack.sent());
    }

    #[test]
    fn stale_sweep_drops_same_named_duplicates_and_keeps_the_live_row() {
        let live = std::collections::HashSet::from(["keep".to_string()]);
        let existing = [
            ("keep", "workflow-schedule:wf:0"),
            ("dup-a", "workflow-schedule:wf:0"),
            ("dup-b", "workflow-schedule:wf:0"),
            ("other", "memory-consolidation"),
        ];
        let mut stale = super::stale_schedule_job_ids(existing, &live);
        stale.sort_unstable();
        assert_eq!(stale, vec!["dup-a", "dup-b"]);
    }

    #[test]
    fn stale_sweep_drops_orphans_of_deleted_workflows() {
        let live = std::collections::HashSet::from(["keep".to_string()]);
        let existing = [
            ("keep", "workflow-schedule:wf:0"),
            ("gone", "workflow-schedule:deleted:0"),
        ];
        assert_eq!(
            super::stale_schedule_job_ids(existing, &live),
            vec!["gone".to_string()]
        );
    }

    #[test]
    fn stale_sweep_never_touches_a_non_workflow_job() {
        let live = std::collections::HashSet::new();
        let existing = [("job", "memory-consolidation"), ("mon", "monitor-sweep")];
        assert!(super::stale_schedule_job_ids(existing, &live).is_empty());
    }

    async fn test_runtime() -> Arc<WorkflowRuntime> {
        super::testing::runtime().await
    }

    #[tokio::test]
    async fn step_without_a_declared_agent_resolves_without_a_registry() {
        let runtime = test_runtime().await;
        let (service, agent_id) = runtime
            .resolve_step_service(None)
            .expect("an unnamed agent must resolve");
        assert_eq!(agent_id, service.agent_id.as_str());
    }

    #[tokio::test]
    async fn step_naming_an_agent_never_falls_back_silently() {
        let runtime = test_runtime().await;
        let err = match runtime.resolve_step_service(Some("example")) {
            Err(err) => err,
            Ok((service, _)) => panic!(
                "a named agent must not resolve to whoever owns the runtime, got '{}'",
                service.agent_id.as_str()
            ),
        };
        assert!(
            err.contains("example"),
            "the error must name the agent, got: {err}"
        );
    }

    /// A step runs only on an agent whose access policy admits the run's
    /// identity, and one it doesn't is refused in the words an unregistered
    /// agent is.
    #[tokio::test]
    async fn a_step_never_runs_on_an_agent_the_run_may_not_use() {
        use workforce_common::settings::{AccessMode, AgentAccessConfig};
        let tmp = tempfile::tempdir().unwrap();
        let runtime = test_runtime().await;
        let gate = crate::access_gate::AccessGate::new(std::collections::HashMap::new());
        let mut agents = std::collections::HashMap::new();
        for id in ["default", "private"] {
            let dir = tmp.path().join(id);
            let mut config = workforce_common::settings::WorkforceConfig::default();
            config.hub.data_dir = dir.to_string_lossy().to_string();
            config.memory.db_path = dir.join("memory.db").to_string_lossy().into_owned();
            config.llm.api_key = Some("test-key".into());
            config.agents = vec![workforce_common::settings::AgentConfig {
                id: id.into(),
                ..Default::default()
            }];
            let service = AgentService::from_config(config, None).await.unwrap();
            assert_eq!(service.agent_id.as_str(), id);
            service.set_access_gate(Arc::clone(&gate));
            agents.insert(id.to_string(), service);
        }
        // After each service has published its own (open) policy.
        gate.upsert(
            "private".into(),
            AgentAccessConfig {
                mode: AccessMode::Allowlist,
                allow: vec!["alice".into()],
                ..Default::default()
            },
        );
        let registry = Arc::new(crate::agent_registry::AgentRegistry::from_parts(
            agents,
            "default".into(),
        ));
        runtime.set_agent_registry(registry);
        let as_ = |who: &'static str, agent: Option<&'static str>| {
            let runtime = Arc::clone(&runtime);
            workforce_agent::runner::CURRENT_USER_ID.scope(who.to_string(), async move {
                runtime.resolve_step_service(agent).map(|(_, id)| id)
            })
        };
        assert_eq!(
            as_("alice", Some("private")).await.as_deref(),
            Ok("private")
        );
        assert_eq!(
            as_("bob", Some("private")).await.unwrap_err(),
            as_("bob", Some("nobody"))
                .await
                .unwrap_err()
                .replace("nobody", "private"),
            "refused as an agent that isn't there"
        );
        assert_eq!(as_("bob", None).await.as_deref(), Ok("default"));
    }

    /// Seats every workspace in the temp dir and keeps what each asked for:
    /// the project, its repos in order, and where rounds start.
    #[derive(Default)]
    struct SeatRecorder(parking_lot::Mutex<Vec<(String, Vec<String>, Option<String>)>>);

    #[async_trait::async_trait]
    impl SandboxLauncher for SeatRecorder {
        async fn ensure(&self, spec: &WorkspaceSpec<'_>) -> Result<Arc<dyn CodingSandbox>> {
            self.0.lock().push((
                spec.project.name.clone(),
                slugs(spec.project),
                spec.primary.map(str::to_string),
            ));
            Ok(crate::coding_sandbox::adopt_workdir(
                std::env::temp_dir(),
                Vec::new(),
                None,
            ))
        }
        async fn exists(&self, _: &str, _: &str, _: &str) -> bool {
            false
        }
        async fn close(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn sweep(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &(dyn for<'a> Fn(&'a str) -> bool + Sync),
        ) {
        }
    }

    #[tokio::test]
    async fn a_workflow_scope_is_prepared_with_every_workspace_repo_and_starts_in_the_target() {
        let runtime = test_runtime().await;
        let recorder = Arc::new(SeatRecorder::default());
        let launcher: Arc<dyn SandboxLauncher> = recorder.clone();
        let picked = instance_with_vars(serde_json::json!({
            "steps": { "pick": {
                "target_repo": "acme/app",
                "workspace_repos": "acme/app acme/WebApi,\n\
                                    acme/worker"
            } }
        }));
        runtime
            .remote_step_sandbox(&launcher, &picked, &runtime.service)
            .await
            .unwrap();

        let clashing = instance_with_vars(serde_json::json!({
            "steps": { "pick": {
                "target_repo": "acme/app",
                "workspace_repos": "fork/app"
            } }
        }));
        let err = runtime
            .remote_step_sandbox(&launcher, &clashing, &runtime.service)
            .await
            .err()
            .expect("two repos sharing a checkout are refused");
        assert!(err.to_string().contains("fork/app"), "{err}");

        assert_eq!(
            recorder.0.lock().clone(),
            [(
                "workflow:acme/app".to_string(),
                vec![
                    "acme/app".to_string(),
                    "acme/WebApi".to_string(),
                    "acme/worker".to_string(),
                ],
                Some("app".to_string()),
            )],
            "one scope, target first, and the refused one never reached the worker"
        );
    }

    #[test]
    fn trigger_delivery_channel_reads_the_payload_channel() {
        let payload = serde_json::json!({ "channel_id": "C0FAKECHAN2" });
        assert_eq!(
            super::trigger_delivery_channel(&payload).as_deref(),
            Some("C0FAKECHAN2")
        );
    }

    #[test]
    fn trigger_delivery_channel_is_absent_when_unset_or_blank() {
        assert!(super::trigger_delivery_channel(&serde_json::json!({})).is_none());
        assert!(
            super::trigger_delivery_channel(&serde_json::json!({ "channel_id": "" })).is_none()
        );
        assert!(super::trigger_delivery_channel(&serde_json::json!({ "channel_id": 7 })).is_none());
    }

    #[test]
    fn artifact_scope_is_per_instance_not_per_step() {
        assert_eq!(super::artifact_scope("inst-1"), "workflow:inst-1");
        assert_ne!(
            super::artifact_scope("inst-1"),
            super::artifact_scope("inst-2")
        );
    }

    use super::testing::RecordingSurface;
    use super::*;
    use workforce_common::channels::SurfaceKey;
    use workforce_orchestrator::workflow::definition::is_valid_http_trigger_id;
    use workforce_orchestrator::workflow::instance::EngineAttempt;
    use workforce_orchestrator::workflow_store::mint_trigger_path;

    #[test]
    fn a_halt_card_names_the_run_and_carries_the_definitions_failure_report() {
        let meta = WorkflowMeta {
            id: "order-review".into(),
            name: "PR review".into(),
            description: String::new(),
            version: "1".into(),
            agent: None,
            context: Default::default(),
            delivery: DeliveryMode::Reports,
            max_concurrent: 1,
            failure_report: Some(
                "{{ steps.pick.ticket_id }} {{ steps.pick.pr_url }} ({{ step }}: {{ error }})"
                    .into(),
            ),
            requires: Default::default(),
            pause_on_usage_cap: false,
            steerable_by_others: true,
        };
        let trigger = TriggerInfo {
            source: TriggerSource::Api,
            payload: serde_json::json!({}),
        };
        let mut instance = WorkflowInstance::new("order-review", trigger, "pick", None);
        instance.id = "8deea821-30c5-4954".into();
        instance.current_step_id = Some("fix".into());
        instance.variables = serde_json::json!({
            "steps": {"pick": {"ticket_id": "ORD-2593", "pr_url": "https://github.com/o/r/pull/1"}}
        });
        let card = failure_card(
            Some(&meta),
            &instance,
            "exit 1",
            "Claude Code failed (exit 1).",
        );
        assert_eq!(
            card,
            "⚠️ *PR review* halted at step 'fix' (instance 8deea821)\n\
             ORD-2593 https://github.com/o/r/pull/1 (fix: exit 1)\n\
             Claude Code failed (exit 1)."
        );

        instance.variables = serde_json::json!({});
        instance.current_step_id = Some("pick".into());
        let early = failure_card(
            Some(&meta),
            &instance,
            "repo_map is empty",
            "Tool step 'pick' failed",
        );
        assert_eq!(
            early,
            "⚠️ *PR review* halted at step 'pick' (instance 8deea821)\n\
             (pick: repo_map is empty)\n\
             Tool step 'pick' failed",
            "a step that never ran reads as empty rather than failing the card"
        );

        let tail = narration_tail(&format!(
            "{}\nnow the failing test\n\nCommitting and pushing:",
            "a".repeat(700)
        ));
        assert!(tail.starts_with("> …"), "{tail}");
        assert!(
            tail.ends_with("> now the failing test\n> Committing and pushing:"),
            "{tail}"
        );
        assert!(!tail.contains(&"a".repeat(650)));
        assert_eq!(narration_tail("  one\n\n two "), "> one\n> two");

        let bare = failure_card(None, &instance, "x", "body");
        assert!(bare.starts_with("⚠️ *order-review* halted at step 'pick'"));

        instance.variables = serde_json::json!("not an object");
        let odd = failure_card(Some(&meta), &instance, "x", "body");
        assert!(
            odd.contains("\n(pick: x)\n"),
            "step and error still reach the template: {odd}"
        );
    }

    #[test]
    fn a_workflow_blocks_a_scheduled_start_only_at_its_cap() {
        let ids: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(blocking(&[], 1), None);
        assert_eq!(blocking(&ids[..1], 1), Some(&ids[0]));
        assert_eq!(blocking(&ids[..1], 2), None);
        assert_eq!(blocking(&ids, 2), Some(&ids[1]));
        assert_eq!(
            blocking(&ids, 0),
            Some(&ids[1]),
            "zero never means unlimited"
        );
    }

    fn def_with_required_context(fields: &[&str]) -> WorkflowDefinition {
        def_with_context(fields.iter().map(|f| (*f, true, "")).collect())
    }

    fn def_with_context(fields: Vec<(&str, bool, &str)>) -> WorkflowDefinition {
        use workforce_orchestrator::workflow::definition::ContextField;
        let mut context = std::collections::HashMap::new();
        for (name, required, description) in fields {
            context.insert(
                name.to_string(),
                ContextField {
                    required,
                    description: description.to_string(),
                },
            );
        }
        WorkflowDefinition {
            workflow: WorkflowMeta {
                max_concurrent: 1,
                failure_report: None,
                id: "feature-impl".into(),
                name: "Test".into(),
                description: String::new(),
                version: "1".into(),
                agent: None,
                context,
                delivery: Default::default(),
                requires: Default::default(),
                pause_on_usage_cap: false,
                steerable_by_others: true,
            },
            steps: Vec::new(),
            triggers: Vec::new(),
            variables: Vec::new(),
        }
    }

    fn def_with_http_trigger(workflow_id: &str, trigger_id: &str) -> WorkflowDefinition {
        use workforce_orchestrator::workflow::definition::TriggerAuth;
        WorkflowDefinition {
            workflow: WorkflowMeta {
                max_concurrent: 1,
                failure_report: None,
                id: workflow_id.into(),
                name: "T".into(),
                description: String::new(),
                version: "1".into(),
                agent: None,
                context: std::collections::HashMap::new(),
                delivery: Default::default(),
                requires: Default::default(),
                pause_on_usage_cap: false,
                steerable_by_others: true,
            },
            steps: Vec::new(),
            triggers: vec![TriggerDefinition::Http {
                id: trigger_id.into(),
                match_rules: std::collections::HashMap::new(),
                auth: TriggerAuth::Bearer {
                    token: "[[vault:t]]".into(),
                },
                response: Default::default(),
            }],
            variables: Vec::new(),
        }
    }

    fn owned(def: WorkflowDefinition, owner: Option<&str>) -> OwnedDefinition {
        OwnedDefinition {
            def,
            owner: owner.map(str::to_string),
            visibility: Visibility::Global,
            last_editor: owner.map(str::to_string),
            run_as: None,
            content_hash: None,
        }
    }

    fn branching_step_with_outputs() -> WorkflowDefinition {
        WorkflowDefinition::from_toml(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\n\n[[step]]\nid = \"implement\"\nname = \"I\"\n\
             task = \"do it\"\ngoto = \"link\"\nbranches = [{ when = \"not-code\", goto = \"skip\" }]\n\
             outputs = [{ name = \"pr_url\" }, { name = \"pr_number\" }]\n\
             [[step]]\nid = \"link\"\nname = \"L\"\ntask = \"link\"\nexit = true\n\
             [[step]]\nid = \"skip\"\nname = \"S\"\ntask = \"skip\"\nexit = true\n",
        )
        .unwrap()
    }

    /// What `assess_handover` asks of a step's response, before any verifier.
    fn handed_on(
        step: &workforce_orchestrator::workflow::definition::StepDefinition,
        response: &str,
        repairs_left: bool,
    ) -> std::result::Result<Option<serde_json::Value>, Refusal> {
        crate::handoff::package_of(
            response,
            &step.outputs,
            branch_outcome(step, response).is_some(),
            repairs_left,
        )
    }

    #[test]
    fn a_step_on_the_fall_through_path_is_sent_back_for_every_absent_output() {
        let def = branching_step_with_outputs();
        let step = def.get_step("implement").unwrap();
        let sent_back = |response: &str| match handed_on(step, response, true) {
            Err(Refusal::SendBack(correction)) => correction.detail,
            other => panic!("expected the step to be sent back, got {other:?}"),
        };

        assert_eq!(
            sent_back("done, no block"),
            "The output block was not accepted: 'pr_url' is missing; 'pr_number' is missing."
        );
        assert_eq!(
            sent_back("OUTPUTS:\npr_url: https://x/pull/1\nEND_OUTPUTS"),
            "The output block was not accepted: 'pr_number' is missing."
        );
        assert!(
            sent_back("OUTPUTS:\npr_url: u\nEND_OUTPUTS\nOUTCOME: unknown-label")
                .contains("'pr_number' is missing")
        );
        assert_eq!(
            handed_on(step, "done, no block", false),
            Err(Refusal::Fail(
                "did not produce a valid output block: 'pr_url' is missing; 'pr_number' is missing"
                    .to_string()
            ))
        );
    }

    #[test]
    fn a_step_leaving_by_a_declared_branch_owes_no_output() {
        let def = branching_step_with_outputs();
        let step = def.get_step("implement").unwrap();

        assert_eq!(
            handed_on(
                step,
                "not a code change\nOUTCOME: not-code — data issue",
                true
            ),
            Ok(None)
        );
        assert_eq!(
            handed_on(
                step,
                "all good\nOUTPUTS:\npr_url: u\npr_number: 1\nEND_OUTPUTS",
                true
            ),
            Ok(Some(serde_json::json!({ "pr_url": "u", "pr_number": "1" })))
        );
    }

    #[test]
    fn a_child_payload_is_an_object_or_nothing() {
        assert_eq!(child_payload("  ").unwrap(), serde_json::json!({}));
        assert_eq!(
            child_payload(r#"{"ticket": "OPS-1"}"#).unwrap(),
            serde_json::json!({"ticket": "OPS-1"})
        );
        assert!(child_payload("[1]").unwrap_err().contains("JSON object"));
        assert!(child_payload("{oops")
            .unwrap_err()
            .contains("not valid JSON"));
    }

    fn child_row(status: &str, variables: serde_json::Value) -> WorkflowInstanceRow {
        WorkflowInstanceRow {
            id: "child-1".into(),
            workflow_id: "alice/child".into(),
            status: status.into(),
            current_step_id: None,
            step_history: serde_json::json!([
                { "step_id": "plan", "outcome": null, "agent_response": "", "rounds": 1, "cost_usd": 0.0,
                  "started_at": "2026-09-26T10:00:00Z", "completed_at": "2026-09-26T10:00:30Z" },
                { "step_id": "implement", "outcome": null, "agent_response": "", "rounds": 1, "cost_usd": 0.0,
                  "started_at": "2026-09-26T10:00:30Z", "completed_at": "2026-09-26T10:01:00Z" }
            ])
            .to_string(),
            variables: variables.to_string(),
            trigger_json: "{}".into(),
            agent_id: None,
            created_at: "2026-09-26T10:00:00Z".into(),
            updated_at: "2026-09-26T10:01:00Z".into(),
            parent_instance_id: Some("parent-1".into()),
            channel_id: None,
            thread_id: None,
            owner_instance: None,
            lease_expires_at: None,
            version: 0,
        }
    }

    #[test]
    fn a_finished_child_hands_back_its_status_reason_and_every_step_output() {
        let row = child_row(
            "completed",
            serde_json::json!({
                "ticket": "OPS-1",
                "steps": {
                    "plan": { "summary": "first", "pr_number": "41" },
                    "implement": { "pr_number": "42", "branch_name": "fix/ops-1" }
                }
            }),
        );
        let out = child_run_output(&row, "child", &["completed".into()]);
        assert_eq!(out["status"], "completed");
        assert_eq!(out["outcome"], "completed");
        assert_eq!(out["instance_id"], "child-1");
        assert_eq!(out["workflow_id"], "child");
        assert_eq!(out["pr_number"], "42", "a later step wins the name");
        assert_eq!(out["summary"], "first");
        assert_eq!(out["steps"]["implement"]["branch_name"], "fix/ops-1");
        assert!(
            out.get("ticket").is_none(),
            "the child's payload is not an output"
        );
        let text = render_tool_step_response(&out, &["pr_number".into()]);
        assert!(text.contains("OUTPUTS:\npr_number: 42\nEND_OUTPUTS"));
        assert!(text.ends_with("OUTCOME: completed"));

        let unclaimed = child_run_output(&row, "child", &["failed".into()]);
        assert!(
            unclaimed.get("outcome").is_none(),
            "a status no branch claims falls through to goto"
        );
        assert!(!render_tool_step_response(&unclaimed, &[]).contains("OUTCOME:"));

        let failed = child_run_output(
            &child_row(
                "failed",
                serde_json::json!({ "_failure_reason": "LLM error" }),
            ),
            "child",
            &["failed".into()],
        );
        assert_eq!(failed["outcome"], "failed");
        assert_eq!(failed["failure_reason"], "LLM error");
        assert!(failed.get("_failure_reason").is_none());
    }

    #[test]
    fn a_run_has_ended_only_in_a_terminal_status() {
        for status in ["completed", "failed", "cancelled", "exited"] {
            assert!(run_ended(status), "{status}");
        }
        for status in ["running", "paused", "queued", "bogus"] {
            assert!(!run_ended(status), "{status}");
        }
    }

    #[test]
    fn a_child_started_note_is_per_step_and_cleared_once_answered() {
        let mut instance = instance_with_vars(serde_json::json!({}));
        note_child_started(&mut instance, "run");
        let note = &instance.variables[CHILD_STARTED_KEY];
        assert_eq!(note["step"], "run");
        assert!(note["since"].as_str().is_some_and(|s| s.contains('T')));
        note_child_instance(&mut instance, "child-9");
        assert_eq!(
            instance.variables[CHILD_STARTED_KEY]["instance_id"],
            "child-9"
        );
        clear_child_started(&mut instance);
        assert!(instance.variables.get(CHILD_STARTED_KEY).is_none());
    }

    #[test]
    fn a_failed_or_cancelled_child_halts_the_caller_unless_a_branch_claims_it() {
        let failed = child_row(
            "failed",
            serde_json::json!({ "_failure_reason": "LLM error" }),
        );
        let out = child_run_output(&failed, "child", &[]);
        let halt = unhandled_child_end(&failed, &out, &[]).unwrap();
        assert!(halt.contains("failed: LLM error"), "{halt}");
        assert!(unhandled_child_end(&failed, &out, &["failed".into()]).is_none());
        let done = child_row("completed", serde_json::json!({}));
        assert!(unhandled_child_end(&done, &child_run_output(&done, "c", &[]), &[]).is_none());
    }

    /// A step resolves its child in its own owner's namespace, then among
    /// system workflows, and never in another member's.
    #[tokio::test]
    async fn a_child_key_resolves_to_the_same_owner_then_system_never_another_member() {
        let runtime = test_runtime().await;
        let def = queueable_def(1);
        {
            let mut defs = runtime.definitions.write().await;
            defs.insert("alice/child".into(), owned(def.clone(), Some("alice")));
            defs.insert("child".into(), owned(def.clone(), None));
            defs.insert("only-system".into(), owned(def.clone(), None));
            defs.insert("bob/secret".into(), owned(def.clone(), Some("bob")));
        }
        assert_eq!(
            runtime
                .resolve_child_key("alice/parent", "child")
                .await
                .as_deref(),
            Some("alice/child")
        );
        assert_eq!(
            runtime
                .resolve_child_key("parent", "child")
                .await
                .as_deref(),
            Some("child")
        );
        assert_eq!(
            runtime
                .resolve_child_key("alice/parent", "only-system")
                .await
                .as_deref(),
            Some("only-system")
        );
        assert_eq!(
            runtime.resolve_child_key("alice/parent", "secret").await,
            None
        );
        assert_eq!(
            runtime
                .resolve_child_key("alice/parent", "bob/secret")
                .await,
            None
        );
    }

    /// After a restart the step must find the child it started even when that
    /// child has since finished — the note carries its id — and, before the id
    /// was noted, a still-running child under this instance since the note.
    #[tokio::test]
    async fn a_resumed_workflow_step_attaches_to_the_child_it_started() {
        let runtime = test_runtime().await;
        let def = queueable_def(1);
        let mut parent = WorkflowExecutor::start(&def, webhook_trigger(), None).unwrap();
        note_child_started(&mut parent, "run");
        let mut finished =
            WorkflowExecutor::start(&def, webhook_trigger(), Some(parent.id.clone())).unwrap();
        finished.workflow_id = "smoke".into();
        finished.status = InstanceStatus::Completed;
        runtime.save_instance(&finished).unwrap();
        assert_eq!(
            runtime.child_started_by(&parent, "run", "smoke"),
            None,
            "a finished child is invisible to the child listing until its id is noted"
        );
        note_child_instance(&mut parent, &finished.id);
        assert_eq!(
            runtime.child_started_by(&parent, "run", "smoke").as_deref(),
            Some(finished.id.as_str())
        );
        assert_eq!(
            runtime.child_started_by(&parent, "other-step", "smoke"),
            None
        );

        let mut parent = WorkflowExecutor::start(&def, webhook_trigger(), None).unwrap();
        note_child_started(&mut parent, "run");
        let mut running =
            WorkflowExecutor::start(&def, webhook_trigger(), Some(parent.id.clone())).unwrap();
        running.workflow_id = "smoke".into();
        runtime.save_instance(&running).unwrap();
        assert_eq!(
            runtime.child_started_by(&parent, "run", "smoke").as_deref(),
            Some(running.id.as_str())
        );
        assert_eq!(
            runtime.child_started_by(&parent, "run", "other-workflow"),
            None
        );
    }

    /// An abort that lands before the child row exists cannot cascade to it;
    /// the waiting step notices the parent's status, cancels the child and
    /// hands back nothing to record.
    #[tokio::test]
    async fn waiting_on_a_child_stops_and_cancels_it_when_the_parent_was_aborted() {
        let runtime = test_runtime().await;
        let def = queueable_def(1);
        let mut parent = WorkflowExecutor::start(&def, webhook_trigger(), None).unwrap();
        parent.status = InstanceStatus::Cancelled;
        runtime.save_instance(&parent).unwrap();
        let child =
            WorkflowExecutor::start(&def, webhook_trigger(), Some(parent.id.clone())).unwrap();
        runtime.save_instance(&child).unwrap();

        let waited = runtime
            .await_child(&parent.id, &child.id, None)
            .await
            .unwrap();
        assert!(waited.is_none());
        assert_eq!(status_of(&runtime, &child.id), InstanceStatus::Cancelled);
        assert!(runtime.cancel_tokens.lock().await.get(&parent.id).is_none());
    }

    #[tokio::test]
    async fn an_operator_cannot_rewrite_who_a_run_acts_as_or_steer_anothers_run() {
        let runtime = test_runtime().await;
        let person = |id: &str| TurnPrincipal::run_as(Some(id.to_string()));
        let nobody = TurnPrincipal::default();
        let mut alices =
            WorkflowExecutor::start(&queueable_def(1), webhook_trigger(), None).unwrap();
        persist_principal(&mut alices, &person("alice"));
        runtime.save_instance(&alices).unwrap();
        let system = WorkflowExecutor::start(&queueable_def(1), webhook_trigger(), None).unwrap();
        runtime.save_instance(&system).unwrap();

        for engine_owned in [RUN_AS, BILL_TO, "_channel_id"] {
            for run in [&alices, &system] {
                assert!(
                    runtime
                        .set_operator_variable(&person("alice"), &run.id, engine_owned, "mallory")
                        .is_err(),
                    "{engine_owned}"
                );
            }
        }
        for other in [&person("admin"), &nobody] {
            assert!(
                runtime
                    .set_operator_variable(other, &alices.id, "region", "emea")
                    .is_err(),
                "an administrator, signed in or not, is not the person the run acts as"
            );
            runtime
                .set_operator_variable(other, &system.id, "region", "emea")
                .unwrap();
        }
        runtime
            .set_operator_variable(&person("alice"), &alices.id, "region", "apac")
            .unwrap();

        let kept = runtime.load_instance(&alices.id).unwrap();
        assert_eq!(instance_principal(&kept).run_as.as_deref(), Some("alice"));
        assert_eq!(kept.variables["region"], "apac");
    }

    #[tokio::test]
    async fn a_busy_decision_step_asks_again_and_fails_only_when_it_stays_busy() {
        use crate::decide::Undecided;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let runtime = test_runtime().await;
        let def = queueable_def(1);
        let run = WorkflowExecutor::start(&def, webhook_trigger(), None).unwrap();
        runtime.save_instance(&run).unwrap();
        let waits = [Duration::from_millis(5); 3];
        let asked = AtomicUsize::new(0);
        let answers_on = |attempt: usize, busy: bool| {
            asked.store(0, Ordering::SeqCst);
            let asked = &asked;
            move || {
                let n = asked.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if n >= attempt {
                        Ok("yes")
                    } else {
                        Err(Undecided {
                            reason: format!("no answer on attempt {n}"),
                            busy,
                        })
                    }
                }
            }
        };

        let third = runtime
            .retry_while_busy(&run.id, &waits, answers_on(3, true))
            .await;
        assert_eq!(third, Ok(Some("yes")));
        assert_eq!(asked.load(Ordering::SeqCst), 3);

        let never = runtime
            .retry_while_busy(&run.id, &waits, answers_on(usize::MAX, true))
            .await;
        assert_eq!(never, Err("no answer on attempt 4".to_string()));
        assert_eq!(asked.load(Ordering::SeqCst), 4, "once, then once per wait");

        let broken = runtime
            .retry_while_busy(&run.id, &waits, answers_on(usize::MAX, false))
            .await;
        assert_eq!(broken, Err("no answer on attempt 1".to_string()));
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "a fault is not asked again"
        );
        assert!(runtime.cancel_tokens.lock().await.get(&run.id).is_none());
    }

    #[tokio::test]
    async fn a_busy_decision_step_stops_waiting_when_its_run_is_stopped() {
        use crate::decide::Undecided;

        let runtime = test_runtime().await;
        let def = queueable_def(1);
        let busy = || async {
            Err::<&str, _>(Undecided {
                reason: "busy".into(),
                busy: true,
            })
        };
        let long = [Duration::from_secs(60)];

        let run = WorkflowExecutor::start(&def, webhook_trigger(), None).unwrap();
        runtime.save_instance(&run).unwrap();
        let started = std::time::Instant::now();
        let abort = async {
            loop {
                if let Some(token) = runtime.cancel_tokens.lock().await.get(&run.id) {
                    break token.cancel();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let (waited, ()) = tokio::join!(runtime.retry_while_busy(&run.id, &long, busy), abort);
        assert_eq!(waited, Ok(None));
        assert!(started.elapsed() < Duration::from_secs(10));

        let mut cancelled = WorkflowExecutor::start(&def, webhook_trigger(), None).unwrap();
        cancelled.status = InstanceStatus::Cancelled;
        runtime.save_instance(&cancelled).unwrap();
        let waited = runtime.retry_while_busy(&cancelled.id, &long, busy).await;
        assert_eq!(waited, Ok(None), "an abort that landed before the wait");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn waiting_on_a_child_returns_it_once_it_ends() {
        let runtime = test_runtime().await;
        let def = queueable_def(1);
        let parent = WorkflowExecutor::start(&def, webhook_trigger(), None).unwrap();
        runtime.save_instance(&parent).unwrap();
        let mut child =
            WorkflowExecutor::start(&def, webhook_trigger(), Some(parent.id.clone())).unwrap();
        child.status = InstanceStatus::Failed;
        runtime.save_instance(&child).unwrap();

        let row = runtime
            .await_child(&parent.id, &child.id, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.id, child.id);
        assert_eq!(row.status, "failed");
        let missing = runtime.await_child(&parent.id, "no-such-child", None).await;
        assert!(missing.unwrap_err().contains("disappeared"));
    }

    #[test]
    fn tool_step_response_renders_outputs_and_outcome() {
        let output = serde_json::json!({
            "enabled": 2,
            "failed": 0,
            "outcome": "partial-failure",
            "detail": {"nested": true},
        });
        let declared = vec!["enabled".to_string(), "missing".to_string()];
        let text = render_tool_step_response(&output, &declared);
        assert!(text.contains("\"enabled\": 2"));
        assert!(text.contains("OUTPUTS:\nenabled: 2\nEND_OUTPUTS"));
        assert!(text.ends_with("OUTCOME: partial-failure"));
        assert_eq!(
            workforce_orchestrator::workflow::engine::parse_outcome(&text).as_deref(),
            Some("partial-failure")
        );
        let parsed =
            workforce_orchestrator::workflow::engine::parse_outputs(&text, &["enabled"]).unwrap();
        assert_eq!(parsed["enabled"], "2");
    }

    #[test]
    fn a_tool_step_task_is_its_json_object_or_a_loud_authoring_error() {
        assert_eq!(tool_step_input("  ").unwrap(), serde_json::json!({}));
        assert_eq!(
            tool_step_input(r#"{"items": [1, 2]}"#).unwrap(),
            serde_json::json!({"items": [1, 2]})
        );
        assert_eq!(
            tool_step_input("summarise this").unwrap(),
            serde_json::json!({"input": "summarise this"})
        );
        assert!(tool_step_input(r#"{"items": a, b}"#)
            .unwrap_err()
            .starts_with("task resolved to invalid JSON"));
        assert!(tool_step_input("[1, 2]").is_ok_and(|v| v["input"] == "[1, 2]"));
    }

    struct AgentIdEcho;

    #[async_trait::async_trait]
    impl workforce_agent::tools::Tool for AgentIdEcho {
        fn name(&self) -> &str {
            "probe.agent"
        }
        fn description(&self) -> &str {
            "reports the agent id in scope"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
        ) -> eyre::Result<workforce_agent::tools::ToolResult> {
            let agent = workforce_agent::runner::CURRENT_AGENT_ID
                .try_with(|id| id.clone())
                .unwrap_or_default();
            Ok(workforce_agent::tools::ToolResult::success(
                serde_json::json!({"agent": agent}),
                1,
            ))
        }
    }

    #[tokio::test]
    async fn a_tool_step_runs_its_tool_under_the_executing_agent_id() {
        let tools = workforce_agent::tools::registry::ToolRegistry::new();
        tools.register(Arc::new(AgentIdEcho));
        let out = dispatch_tool_step(
            &tools,
            "example",
            "probe.agent",
            serde_json::json!({}),
            &["agent".to_string()],
            workforce_agent::tools::arguments::ArgumentValidation::Shadow,
        )
        .await
        .unwrap();
        assert!(out.contains("agent: example"), "{out}");
        let missing = dispatch_tool_step(
            &tools,
            "example",
            "probe.none",
            serde_json::json!({}),
            &[],
            workforce_agent::tools::arguments::ArgumentValidation::Shadow,
        )
        .await
        .unwrap_err();
        assert_eq!(missing, "tool 'probe.none' is not registered");
    }

    /// A tool that refuses to run without a human, and counts every time it
    /// is reached — the assertion is that it never is.
    struct GatedProbe(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl workforce_agent::tools::Tool for GatedProbe {
        fn name(&self) -> &str {
            "record-keeper.apply_change"
        }
        fn description(&self) -> &str {
            "apply a change"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn approval_requirement(&self) -> Option<String> {
            Some("Required by the skill's own approval gate".to_string())
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
        ) -> eyre::Result<workforce_agent::tools::ToolResult> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(workforce_agent::tools::ToolResult::success(
                serde_json::json!({"applied": true}),
                1,
            ))
        }
    }

    #[tokio::test]
    async fn an_enforced_tool_step_with_bad_arguments_does_not_run() {
        struct Counting(Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl workforce_agent::tools::Tool for Counting {
            fn name(&self) -> &str {
                "probe.count"
            }
            fn description(&self) -> &str {
                "count"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": { "n": { "type": "integer" } },
                    "required": ["n"],
                })
            }
            async fn execute(
                &self,
                input: serde_json::Value,
            ) -> eyre::Result<workforce_agent::tools::ToolResult> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(workforce_agent::tools::ToolResult::success(input, 1))
            }
        }
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = workforce_agent::tools::registry::ToolRegistry::new();
        tools.register(Arc::new(Counting(Arc::clone(&ran))));
        let enforce = workforce_agent::tools::arguments::ArgumentValidation::Enforce;

        let refused = dispatch_tool_step(
            &tools,
            "example",
            "probe.count",
            serde_json::json!({ "n": "x" }),
            &[],
            enforce,
        )
        .await
        .unwrap_err();
        assert!(
            refused.starts_with("'probe.count' did not run"),
            "{refused}"
        );
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 0);

        dispatch_tool_step(
            &tools,
            "example",
            "probe.count",
            serde_json::json!({ "n": 2 }),
            &[],
            enforce,
        )
        .await
        .expect("valid arguments run");
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A workflow runs unattended, so a tool that requires a human decision
    /// cannot run in a tool step: the step fails rather than executing.
    #[tokio::test]
    async fn a_tool_step_refuses_a_tool_that_requires_human_approval() {
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = workforce_agent::tools::registry::ToolRegistry::new();
        tools.register(Arc::new(GatedProbe(Arc::clone(&ran))));
        tools.register(Arc::new(AgentIdEcho));

        let refused = dispatch_tool_step(
            &tools,
            "example",
            "record-keeper.apply_change",
            serde_json::json!({}),
            &[],
            workforce_agent::tools::arguments::ArgumentValidation::Shadow,
        )
        .await
        .unwrap_err();
        assert!(refused.contains("record-keeper.apply_change"), "{refused}");
        assert!(refused.contains("approve"), "{refused}");
        assert_eq!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the gated tool must not have executed"
        );

        // The control: an ungated tool step is untouched.
        dispatch_tool_step(
            &tools,
            "example",
            "probe.agent",
            serde_json::json!({}),
            &[],
            workforce_agent::tools::arguments::ArgumentValidation::Shadow,
        )
        .await
        .expect("an ungated tool step still runs");
    }

    #[test]
    fn saving_a_workflow_notices_tool_steps_that_need_an_approver() {
        let def = WorkflowDefinition::from_toml(
            "[workflow]\nid = \"wf\"\nname = \"WF\"\n\n             [[step]]\nid = \"apply\"\nname = \"Apply\"\ntool = \"record-keeper.apply_change\"\ntask = \"{}\"\n\n             [[step]]\nid = \"log\"\nname = \"Log\"\ntool = \"record-keeper.write_note\"\ntask = \"{}\"\nexit = true\n",
        )
        .unwrap();
        let skill = |requires_approval: bool| workforce_wasm_runtime::InstalledWasmSkill {
            manifest: workforce_wasm_runtime::SkillManifest {
                name: "record-keeper".into(),
                version: "0.1.0".into(),
                description: "records".into(),
                author: String::new(),
                license: String::new(),
                min_workforce_version: None,
                capabilities: Default::default(),
                tools: vec![
                    workforce_wasm_runtime::manifest::ManifestTool {
                        name: "apply_change".into(),
                        description: "apply".into(),
                        requires_approval,
                        approval_summary: requires_approval.then(|| "apply it".into()),
                        input_schema: serde_json::json!({"type": "object"}),
                        handler: None,
                    },
                    workforce_wasm_runtime::manifest::ManifestTool {
                        name: "write_note".into(),
                        description: "note".into(),
                        requires_approval: false,
                        approval_summary: None,
                        input_schema: serde_json::json!({"type": "object"}),
                        handler: None,
                    },
                ],
                credentials: vec![],
                config_schema: vec![],
                allowed_hosts: vec![],
                permissions: vec![],
            },
            state: workforce_wasm_runtime::WasmSkillState::Active,
            installed_at: Utc::now(),
            source: "test".into(),
            config: HashMap::new(),
            binary_version: 1,
            sha256: String::new(),
            owner: None,
            visibility: Visibility::Global,
            shares: vec![],
        };
        assert_eq!(
            approval_gated_tool_steps(&def, &[skill(true)]),
            vec!["record-keeper.apply_change".to_string()]
        );
        assert!(approval_gated_tool_steps(&def, &[skill(false)]).is_empty());
        assert!(approval_gated_tool_steps(&def, &[]).is_empty());
    }

    #[tokio::test]
    async fn a_tool_step_reads_pipeline_runs_and_facts_as_a_member_under_the_pipeline_agent() {
        let runtime = test_runtime().await;
        let def = WorkflowDefinition::from_toml(
            "[workflow]\nid = \"order-intake\"\nname = \"Order intake\"\nagent = \"example\"\n\n\
             [[step]]\nid = \"pick\"\nname = \"Pick\"\ntool = \"item-picker.claim_next\"\ntask = \"{}\"\nexit = true\n",
        )
        .unwrap();
        runtime
            .definitions
            .write()
            .await
            .insert("order-intake".into(), system_def(def));
        let now = Utc::now().to_rfc3339();
        runtime
            .telemetry_store
            .save_workflow_instance(&WorkflowInstanceRow {
                id: "run-1".into(),
                workflow_id: "order-intake".into(),
                status: "running".into(),
                step_history: "[]".into(),
                variables: "{}".into(),
                trigger_json: "{}".into(),
                created_at: now.clone(),
                updated_at: now,
                ..Default::default()
            })
            .unwrap();
        let store: Arc<dyn crate::store::DocumentStore> =
            Arc::new(crate::store::MemoryDocumentStore::new());
        crate::pipeline::define(
            store.as_ref(),
            &crate::store::Scope::Agent("example".into()),
            "orders",
            &crate::pipeline::fixtures::orders(),
            None,
            None,
        )
        .await
        .unwrap();
        let tools = workforce_agent::tools::registry::ToolRegistry::new();
        crate::pipeline_tool::register_hub_reads(
            &tools,
            store,
            Arc::new(|| false),
            Arc::clone(&runtime) as Arc<dyn crate::pipeline::HubReads>,
        );
        let as_member = |agent: &'static str, tool: &'static str, input: serde_json::Value| {
            workforce_agent::runner::CURRENT_USER_ID.scope(
                "member-1".into(),
                dispatch_tool_step(
                    &tools,
                    agent,
                    tool,
                    input,
                    &[],
                    workforce_agent::tools::arguments::ArgumentValidation::Shadow,
                ),
            )
        };
        let read = |text: String| serde_json::from_str::<serde_json::Value>(&text).unwrap();

        let runs = read(
            as_member(
                "example",
                "pipeline.runs",
                serde_json::json!({"pipeline": "orders", "workflows": ["order-intake"], "statuses": ["running"]}),
            )
            .await
            .unwrap(),
        );
        assert_eq!(runs["count"], 1);
        assert_eq!(
            runs["runs"][0]["agent_id"], "example",
            "a run that recorded no agent runs as the agent its workflow declares"
        );

        let facts = read(
            as_member(
                "example",
                "pipeline.facts",
                serde_json::json!({"pipeline": "orders"}),
            )
            .await
            .unwrap(),
        );
        assert_eq!(facts["workflows"][0]["id"], "order-intake");
        assert_eq!(
            facts["workflows"][0]["tools"],
            serde_json::json!(["item-picker.claim_next"])
        );
        assert!(facts["build"]["commit_sha"].is_string());

        let refused = as_member(
            "quill",
            "pipeline.facts",
            serde_json::json!({"scope": "agent:example", "pipeline": "orders"}),
        )
        .await
        .unwrap_err();
        assert!(refused.contains("belongs to another agent"), "{refused}");
    }

    #[test]
    fn tool_step_response_without_object_output_is_plain() {
        let text = render_tool_step_response(&serde_json::json!("done"), &[]);
        assert_eq!(text, "\"done\"");
    }

    #[test]
    fn principal_round_trips_through_instance_variables() {
        let trigger = TriggerInfo {
            source: TriggerSource::Api,
            payload: serde_json::json!({"emails": ["a@x.com"]}),
        };
        let mut instance = WorkflowInstance::new("wf", trigger, "step1", None);

        persist_principal(&mut instance, &TurnPrincipal::default());
        assert_eq!(instance_principal(&instance), TurnPrincipal::default());

        let principal = TurnPrincipal {
            run_as: Some("heartbeat".into()),
            bill_to: Some("0b5e1f4e-1111-2222-3333-444455556666".into()),
        };
        persist_principal(&mut instance, &principal);
        assert_eq!(instance_principal(&instance), principal);
        assert_eq!(instance.variables["emails"][0], "a@x.com");
    }

    #[tokio::test]
    async fn workflow_summaries_come_back_sorted_by_id_then_owner() {
        let runtime = test_runtime().await;
        let def = |id: &str| {
            WorkflowDefinition::from_toml(&format!(
                "[workflow]\nid = \"{id}\"\nname = \"N\"\n\n[[step]]\nid = \"s\"\nname = \"S\"\ntask = \"t\"\nexit = true\n"
            ))
            .unwrap()
        };
        {
            let mut defs = runtime.definitions.write().await;
            for (key, id, owner) in [
                ("zeta", "zeta", None),
                ("bob/alpha", "alpha", Some("bob")),
                ("alpha", "alpha", None),
                ("mid", "mid", None),
            ] {
                defs.insert(key.into(), owned(def(id), owner));
            }
        }
        let order: Vec<(String, Option<String>)> = runtime
            .list_definitions()
            .await
            .into_iter()
            .map(|summary| (summary.id, summary.owner))
            .collect();
        assert_eq!(
            order,
            [
                ("alpha".to_string(), None),
                ("alpha".to_string(), Some("bob".to_string())),
                ("mid".to_string(), None),
                ("zeta".to_string(), None),
            ]
        );
    }

    fn queueable_def(max_concurrent: u32) -> WorkflowDefinition {
        WorkflowDefinition::from_toml(&format!(
            "[workflow]\nid = \"smoke\"\nname = \"Smoke\"\nmax_concurrent = {max_concurrent}\n\n\
             [[step]]\nid = \"run\"\nname = \"R\"\ntask = \"go\"\nexit = true\n"
        ))
        .unwrap()
    }

    fn webhook_trigger() -> TriggerInfo {
        TriggerInfo {
            source: TriggerSource::Api,
            payload: serde_json::json!({}),
        }
    }

    /// A runtime whose `smoke` workflow (cap `max_concurrent`) already has one
    /// instance saved straight into the store as `running` — no executor, so
    /// nothing can finish behind the test's back. Returns the occupant's id.
    async fn runtime_with_occupant(max_concurrent: u32) -> (Arc<WorkflowRuntime>, String) {
        let runtime = test_runtime().await;
        let def = queueable_def(max_concurrent);
        runtime
            .definitions
            .write()
            .await
            .insert("smoke".into(), owned(def.clone(), None));
        let occupant = WorkflowExecutor::start(&def, webhook_trigger(), None).unwrap();
        runtime.save_instance(&occupant).unwrap();
        (runtime, occupant.id)
    }

    async fn trigger_start(runtime: &Arc<WorkflowRuntime>) -> String {
        runtime
            .start_workflow_from_trigger("smoke", webhook_trigger(), None, None, None, None)
            .await
            .unwrap()
    }

    fn status_of(runtime: &WorkflowRuntime, id: &str) -> InstanceStatus {
        runtime.load_instance(id).unwrap().status
    }

    /// The sweep clears a run's hold before resuming it, so a resume that
    /// fails must put the hold back. Without that the instance sits `paused`
    /// with no marker: this sweep never looks at it again, and neither does
    /// anything else. Here the definition is missing, which is the shape of
    /// every resume refusal — a held lease, a deleted workflow, a gate.
    #[tokio::test]
    async fn a_held_run_keeps_its_hold_when_the_resume_is_refused() {
        let runtime = test_runtime().await;
        let def = queueable_def(1);
        // Deliberately not registered in `definitions`, so resume refuses.
        let mut instance = WorkflowExecutor::start(&def, webhook_trigger(), None).unwrap();
        park_for_usage_cap(&mut instance, "You've reached your Opus limit");
        runtime.save_instance(&instance).unwrap();
        let id = instance.id.clone();

        runtime.resume_usage_capped().await;

        let after = runtime.load_instance(&id).unwrap();
        assert_eq!(
            after.status,
            InstanceStatus::Paused,
            "a refused resume must leave the run held, not running or failed"
        );
        assert!(
            held_for_usage_cap(&after.variables),
            "the hold must be restored, or no later sweep ever picks this run up again"
        );
        assert_eq!(
            after.variables["_usage_cap_reason"],
            serde_json::Value::String("You've reached your Opus limit".to_string()),
            "the original refusal is preserved, not replaced with an empty one"
        );
    }

    /// A runtime whose agent knows `status`'s person, and a run of the
    /// smoke workflow acting as them.
    async fn run_acting_as(
        status: workforce_identity::types::IdentityStatus,
    ) -> (
        Arc<WorkflowRuntime>,
        WorkflowInstance,
        Arc<workforce_identity::sqlite::SqliteIdentityStore>,
        workforce_identity::types::Identity,
    ) {
        use workforce_identity::store::IdentityStore;
        let runtime = test_runtime().await;
        let person = workforce_identity::types::Identity::new(
            "Ada",
            workforce_identity::types::IdentityRole::Member,
        )
        .with_status(status);
        let store = Arc::new(workforce_identity::sqlite::SqliteIdentityStore::in_memory().unwrap());
        store.create(&person).unwrap();
        runtime
            .service
            .set_identity_store(Arc::clone(&store) as Arc<dyn IdentityStore>);
        let def = queueable_def(1);
        let mut instance = WorkflowExecutor::start(&def, webhook_trigger(), None).unwrap();
        instance.variables[RUN_AS] = serde_json::json!(person.id.to_string());
        (runtime, instance, store, person)
    }

    #[tokio::test]
    async fn a_run_acting_as_a_suspended_person_is_held_before_its_next_step() {
        let (runtime, instance, _, _) =
            run_acting_as(workforce_identity::types::IdentityStatus::Suspended).await;
        runtime
            .definitions
            .write()
            .await
            .insert("smoke".into(), owned(queueable_def(1), None));
        runtime.save_instance(&instance).unwrap();

        runtime
            .execute_loop(&instance.id, None, None, None)
            .await
            .unwrap();

        let after = runtime.load_instance(&instance.id).unwrap();
        assert_eq!(after.status, InstanceStatus::Paused);
        assert!(held_for_suspension(&after.variables));
        assert!(after.step_history.is_empty(), "no step ran");
    }

    /// A step the suspension stopped is held to rerun; one that broke for
    /// another reason while its person happened to be suspended fails as it
    /// always did, so reactivation does not rerun it.
    #[tokio::test]
    async fn only_the_suspension_holds_a_failed_step() {
        let (runtime, instance, _, person) =
            run_acting_as(workforce_identity::types::IdentityStatus::Suspended).await;
        for (reason, held) in [
            (
                format!("Turn refused: {}", crate::standing::refusal(&person)),
                true,
            ),
            ("the API returned 500".to_string(), false),
        ] {
            let mut run = WorkflowInstance {
                id: uuid::Uuid::new_v4().to_string(),
                ..instance.clone()
            };
            runtime.save_instance(&run).unwrap();
            runtime
                .halt(&mut run, &reason, "card", None, None, None, None)
                .await
                .unwrap();
            let after = runtime.load_instance(&run.id).unwrap();
            assert_eq!(held_for_suspension(&after.variables), held, "{reason}");
            let expected = match held {
                true => InstanceStatus::Paused,
                false => InstanceStatus::Failed,
            };
            assert_eq!(after.status, expected, "{reason}");
        }
    }

    #[tokio::test]
    async fn a_suspended_person_cannot_start_a_run_by_hand() {
        let (runtime, _, _, person) =
            run_acting_as(workforce_identity::types::IdentityStatus::Suspended).await;
        runtime
            .definitions
            .write()
            .await
            .insert("smoke".into(), owned(queueable_def(1), None));
        let started = crate::user_config::as_principal(
            &TurnPrincipal::run_as(Some(person.id.to_string())),
            runtime.start_workflow("smoke", serde_json::json!({}), None, None, None, None),
        )
        .await;
        assert_eq!(
            started.unwrap_err().to_string(),
            "Ada is suspended, so nothing can run as them."
        );
        assert!(runtime.list_instances(None, None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_held_run_waits_for_its_person_and_keeps_its_hold_when_the_resume_is_refused() {
        use workforce_identity::store::IdentityStore;
        let (runtime, mut instance, store, mut person) =
            run_acting_as(workforce_identity::types::IdentityStatus::Suspended).await;
        park_for_suspension(&mut instance);
        runtime.save_instance(&instance).unwrap();

        runtime.resume_suspension_held().await;
        let waiting = runtime.load_instance(&instance.id).unwrap();
        assert_eq!(waiting.status, InstanceStatus::Paused);
        assert!(held_for_suspension(&waiting.variables));

        person.status = workforce_identity::types::IdentityStatus::Active;
        store.update(&person).unwrap();
        // The definition is not registered, so the resume is refused: the
        // hold must go back on for a later sweep.
        runtime.resume_suspension_held().await;
        let refused = runtime.load_instance(&instance.id).unwrap();
        assert_eq!(refused.status, InstanceStatus::Paused);
        assert!(held_for_suspension(&refused.variables));
    }

    #[tokio::test]
    async fn trigger_start_at_max_concurrent_queues_instead_of_running() {
        let (runtime, occupant) = runtime_with_occupant(1).await;
        let queued = trigger_start(&runtime).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(status_of(&runtime, &queued), InstanceStatus::Queued);
        assert!(
            !runtime.active_loops.lock().contains(&queued),
            "a queued instance must have no executor"
        );
        assert_eq!(
            runtime.active_instance_ids("smoke").unwrap(),
            vec![occupant]
        );
    }

    #[tokio::test]
    async fn trigger_start_below_the_cap_runs_as_before() {
        let (runtime, occupant) = runtime_with_occupant(2).await;
        let started = trigger_start(&runtime).await;
        assert_eq!(status_of(&runtime, &started), InstanceStatus::Running);
        let mut active = runtime.active_instance_ids("smoke").unwrap();
        active.sort();
        let mut expected = vec![occupant, started];
        expected.sort();
        assert_eq!(active, expected);
    }

    #[tokio::test]
    async fn queued_instances_hold_no_capacity_slot_and_round_trip_the_store() {
        let (runtime, occupant) = runtime_with_occupant(1).await;
        let mut instance = runtime.load_instance(&occupant).unwrap();
        instance.status = InstanceStatus::Queued;
        runtime.save_instance(&instance).unwrap();

        assert!(runtime.active_instance_ids("smoke").unwrap().is_empty());
        assert_eq!(runtime.blocking_instance("smoke").await.unwrap(), None);
        let rows = runtime
            .list_instances(Some("queued"), Some("smoke"))
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "queued");
        assert_eq!(status_of(&runtime, &occupant), InstanceStatus::Queued);
    }

    #[tokio::test]
    async fn a_finished_sibling_promotes_the_oldest_queued_instance_first() {
        let (runtime, occupant) = runtime_with_occupant(1).await;
        let first = trigger_start(&runtime).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        let second = trigger_start(&runtime).await;
        assert_eq!(status_of(&runtime, &first), InstanceStatus::Queued);
        assert_eq!(status_of(&runtime, &second), InstanceStatus::Queued);

        let mut done = runtime.load_instance(&occupant).unwrap();
        done.status = InstanceStatus::Completed;
        runtime.save_instance(&done).unwrap();
        runtime.promote_queued("smoke").await;

        assert_eq!(status_of(&runtime, &first), InstanceStatus::Running);
        assert_eq!(status_of(&runtime, &second), InstanceStatus::Queued);
        assert_eq!(runtime.active_instance_ids("smoke").unwrap(), vec![first]);
    }

    fn requiring_def(max_concurrent: u32) -> WorkflowDefinition {
        WorkflowDefinition::from_toml(&format!(
            "[workflow]\nid = \"smoke\"\nname = \"Smoke\"\nmax_concurrent = {max_concurrent}\n\
             requires = {{ \"absent-skill\" = \">=0.10.0\" }}\n\n\
             [[step]]\nid = \"run\"\nname = \"R\"\ntask = \"go\"\nexit = true\n"
        ))
        .unwrap()
    }

    mod coding_rounds {
        use super::*;
        use crate::claude_code::RoundKey;
        use crate::coding_testing::{Rounds, Seats};
        use workforce_agent::turn_journal::TurnJournal;

        struct Run {
            runtime: Arc<WorkflowRuntime>,
            engine: Arc<Rounds>,
            seats: Arc<Seats>,
            def: WorkflowDefinition,
        }

        async fn run(unkept: bool) -> Run {
            run_with(unkept, Rounds::default()).await
        }

        async fn run_with(unkept: bool, engine: Rounds) -> Run {
            run_of(
                "[workflow]\nid = \"build\"\nname = \"Build\"\n\n\
                 [[step]]\nid = \"code\"\nname = \"Code\"\ntask = \"fix it\"\n\
                 engine = \"claude-code\"\nexit = true\n",
                unkept,
                engine,
            )
            .await
        }

        async fn run_def(def: &str, engine: Rounds) -> Run {
            run_of(def, false, engine).await
        }

        async fn run_of(def: &str, unkept: bool, engine: Rounds) -> Run {
            let runtime = test_runtime().await;
            let def = WorkflowDefinition::from_toml(def).unwrap();
            runtime
                .definitions
                .write()
                .await
                .insert("build".into(), owned(def.clone(), None));
            let engine = Arc::new(engine);
            runtime.set_coding_engines(
                Arc::new(coding_engine::Engines::only(engine.clone())),
                CodingEngineConfig::default(),
            );
            let seats = Arc::new(Seats {
                unkept,
                ..Default::default()
            });
            runtime.set_remote_launcher(seats.clone());
            Run {
                runtime,
                engine,
                seats,
                def,
            }
        }

        fn note(instance: &WorkflowInstance) -> Option<serde_json::Value> {
            instance.variables.get("_coding_round").cloned()
        }

        impl Run {
            /// A fresh run, saved, not yet started.
            fn saved(&self) -> String {
                let instance = WorkflowExecutor::start(&self.def, webhook_trigger(), None).unwrap();
                self.runtime.save_instance(&instance).unwrap();
                instance.id
            }

            /// A run whose step was cut short with its round noted at `mark`.
            fn interrupted(&self, key: &RoundKey, mark: Option<&str>) -> String {
                let mut instance =
                    WorkflowExecutor::start(&self.def, webhook_trigger(), None).unwrap();
                instance.variables["_coding_round"] = serde_json::json!({
                    "step": "code",
                    "round": key.round,
                    "started_at": key.started_at,
                    "checkpoint": mark,
                });
                self.runtime.save_instance(&instance).unwrap();
                instance.id
            }

            /// Run `probe` on the run's saved instance while its round runs.
            fn mid_round(
                &self,
                id: &str,
                probe: impl Fn(&WorkflowRuntime, &str) + Send + Sync + 'static,
            ) {
                let runtime = Arc::clone(&self.runtime);
                let id = id.to_string();
                *self.engine.probe.lock().unwrap() = Some(Box::new(move || probe(&runtime, &id)));
            }

            /// The run's note as its round saw it.
            fn note_mid_round(&self, id: &str) -> Arc<std::sync::Mutex<Option<serde_json::Value>>> {
                let seen = Arc::new(std::sync::Mutex::new(Some(serde_json::Value::Null)));
                let noted = Arc::clone(&seen);
                self.mid_round(id, move |runtime, id| {
                    *noted.lock().unwrap() = note(&runtime.load_instance(id).unwrap());
                });
                seen
            }

            async fn pick_up(&self, id: &str) -> WorkflowInstance {
                self.runtime
                    .execute_loop(id, None, None, None)
                    .await
                    .unwrap();
                settled(&self.runtime, id).await
            }
        }

        #[tokio::test]
        async fn a_coding_step_notes_its_round_and_the_job_it_reached_until_its_result_is_kept() {
            let run = run(false).await;
            let id = run.saved();
            let seen = run.note_mid_round(&id);

            let done = run.pick_up(&id).await;

            let runs = run.engine.runs();
            assert_eq!(runs.len(), 1);
            assert!(!runs[0].1, "a new round is not pinned");
            let mid_round = seen.lock().unwrap().take().expect("the round was noted");
            assert_eq!(mid_round["step"], "code");
            assert_eq!(mid_round["checkpoint"], runs[0].0.as_str());
            assert_eq!(done.status, InstanceStatus::Completed);
            assert!(note(&done).is_none());
        }

        #[tokio::test]
        async fn a_run_picked_up_after_a_restart_collects_its_round_at_the_job_it_reached() {
            let run = run(false).await;
            let key = RoundKey::new();
            let mark = format!("{}.0.1", key.round);
            let id = run.interrupted(&key, Some(&mark));

            let done = run.pick_up(&id).await;

            assert_eq!(run.engine.runs(), vec![(mark, true, None)]);
            assert_eq!(done.status, InstanceStatus::Completed);
            assert!(note(&done).is_none());
        }

        #[tokio::test]
        async fn a_round_whose_outcome_nobody_can_tell_halts_its_step_instead_of_running_again() {
            let run = run(false).await;
            let id = run.interrupted(&RoundKey::new(), Some("another-round.0.0"));

            let halted = run.pick_up(&id).await;

            assert!(run.engine.seen.lock().unwrap().is_empty());
            assert_eq!(halted.status, InstanceStatus::Failed);
            let reason = halted.variables["_failure_reason"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            assert!(reason.contains("outcome is unknown"), "{reason}");
            assert!(note(&halted).is_none());
            assert_eq!(
                *run.seats.ended.lock().unwrap(),
                vec![(format!("wf-{id}"), "another-round.0.0".to_string())],
                "the job it left is ended"
            );
        }

        #[tokio::test]
        async fn a_worker_that_does_not_keep_jobs_runs_the_step_unnoted() {
            let run = run(true).await;
            let id = run.saved();
            let seen = run.note_mid_round(&id);

            let done = run.pick_up(&id).await;

            assert!(run.engine.runs().is_empty(), "its round is not keyed");
            assert_eq!(run.engine.seen.lock().unwrap().len(), 1);
            assert_eq!(*seen.lock().unwrap(), None, "nothing was noted mid-round");
            assert_eq!(done.status, InstanceStatus::Completed);
        }

        const COUNTED: &str = "[workflow]\nid = \"build\"\nname = \"Build\"\n\n\
             [[step]]\nid = \"code\"\nname = \"Code\"\ntask = \"count the tables\"\n\
             engine = \"claude-code\"\nexit = true\nhandoff = \"review\"\n\
             outputs = [{ name = \"count\", type = \"integer\" }]\n";
        const THREE: &str = "Counted.\nOUTPUTS:\ncount: 3\nEND_OUTPUTS";
        const FIVE: &str = "Counted.\nOUTPUTS:\ncount: 5\nEND_OUTPUTS";

        /// The task each round of the run's engine was given.
        fn tasks(run: &Run) -> Vec<String> {
            let seen = run.engine.seen.lock().unwrap();
            seen.iter().map(|(round, _)| round.task.clone()).collect()
        }

        fn failure(instance: &WorkflowInstance) -> &str {
            instance.variables["_failure_reason"].as_str().unwrap()
        }

        #[tokio::test]
        async fn a_step_whose_package_does_not_read_is_sent_back_once_and_then_hands_it_on() {
            let run = run_def(COUNTED, Rounds::answering(&["There are three.", THREE])).await;
            let id = run.saved();

            let done = run.pick_up(&id).await;

            assert_eq!(done.status, InstanceStatus::Exited);
            assert_eq!(done.variables["steps"]["code"]["count"], 3);
            assert_eq!(
                done.variables["_handoff"]["package"],
                serde_json::json!({ "count": 3 })
            );
            let causes: Vec<_> = done
                .step_history
                .iter()
                .map(|r| r.correction.as_ref().map(|c| c.cause))
                .collect();
            assert_eq!(causes, [Some(CorrectionCause::Malformed), None]);
            let sent_back = &done.step_history[0];
            assert!(
                sent_back.failed && sent_back.outcome.is_none(),
                "a sent-back run is a failed attempt, which a hub from before corrections skips"
            );
            let tasks = tasks(&run);
            assert_eq!(tasks.len(), 2);
            assert!(!tasks[0].contains("## Correction required"));
            assert!(tasks[1].contains(
                "## Correction required\n\nYour previous attempt was sent back. \
                 The output block was not accepted: 'count' is missing."
            ));
        }

        #[tokio::test]
        async fn a_step_that_never_produces_its_package_fails_after_its_repairs() {
            let run = run_def(COUNTED, Rounds::answering(&["count: lots"; 4])).await;
            let id = run.saved();

            let done = run.pick_up(&id).await;

            assert_eq!(done.status, InstanceStatus::Failed);
            assert_eq!(tasks(&run).len(), 3, "the first run and two repairs");
            assert_eq!(
                failure(&done),
                "Step 'code' did not produce a valid output block: 'count' is missing"
            );
            assert!(done.variables.get("_handoff").is_none());
        }

        /// A verifier that gives each scripted verdict in turn and keeps what
        /// it was shown.
        struct Verdicts {
            verdicts: std::sync::Mutex<std::collections::VecDeque<crate::handoff::Verdict>>,
            shown: std::sync::Mutex<Vec<String>>,
        }

        impl Verdicts {
            fn new(verdicts: Vec<crate::handoff::Verdict>) -> Arc<Self> {
                Arc::new(Self {
                    verdicts: std::sync::Mutex::new(verdicts.into()),
                    shown: std::sync::Mutex::default(),
                })
            }
        }

        #[async_trait::async_trait]
        impl Verifier for Verdicts {
            async fn verify(
                &self,
                brief: &crate::handoff::Brief<'_>,
                output: &str,
            ) -> crate::handoff::Verdict {
                self.shown.lock().unwrap().push(format!(
                    "{}\n{}\n{output}",
                    brief.request,
                    brief.criteria.join("; ")
                ));
                self.verdicts
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("the verifier was called more often than scripted")
            }

            fn spent(&self) -> f64 {
                0.25
            }
        }

        fn rejected() -> crate::handoff::Verdict {
            crate::handoff::Verdict::Rejected(vec![crate::claim_verifier::Claim {
                text: "count matches the schema".into(),
                tier: crate::claim_verifier::Tier::Contradicted,
                evidence: "the schema has three tables".into(),
            }])
        }

        async fn verified_run(
            verify: &str,
            answers: &[&'static str],
            verdicts: Vec<crate::handoff::Verdict>,
        ) -> (Run, Arc<Verdicts>) {
            let run = run_def(
                &format!("{COUNTED}verify = {verify}\n"),
                Rounds::answering(answers),
            )
            .await;
            let verifier = Verdicts::new(verdicts);
            run.runtime.set_verifier(verifier.clone());
            (run, verifier)
        }

        #[tokio::test]
        async fn a_rejected_step_is_corrected_with_the_verdicts_and_then_passes() {
            let (run, verifier) = verified_run(
                "{ criteria = [\"count matches the schema\"] }",
                &[FIVE, THREE],
                vec![rejected(), crate::handoff::Verdict::Passed],
            )
            .await;
            let id = run.saved();

            let done = run.pick_up(&id).await;

            assert_eq!(done.status, InstanceStatus::Exited);
            assert_eq!(done.variables["steps"]["code"]["count"], 3);
            let sent_back = &done.step_history[0];
            assert_eq!(
                sent_back.correction.as_ref().unwrap().cause,
                CorrectionCause::Rejected
            );
            assert_eq!(sent_back.outputs, Some(serde_json::json!({ "count": 5 })));
            assert!((done.step_history[1].cost_usd - 0.25).abs() < 1e-9);
            let tasks = tasks(&run);
            assert_eq!(tasks.len(), 2);
            assert!(tasks[1].contains("- count matches the schema: the schema has three tables"));
            assert!(tasks[1].contains("What it produced:\ncount: 5"));
            let shown = verifier.shown.lock().unwrap();
            assert_eq!(shown.len(), 2, "one verification per output");
            assert_eq!(
                shown[0],
                "count the tables\ncount matches the schema\n{\n  \"count\": 5\n}"
            );
            assert_eq!(
                shown[1], "count the tables\ncount matches the schema\n{\n  \"count\": 3\n}",
                "the verifier is shown the task and the new package, not the prompt around \
                 them or the attempt it rejected"
            );
        }

        #[tokio::test]
        async fn a_step_still_rejected_after_its_correction_round_fails_with_the_verdicts() {
            let (run, verifier) =
                verified_run("true", &[FIVE, FIVE, FIVE], vec![rejected(), rejected()]).await;
            let id = run.saved();

            let done = run.pick_up(&id).await;

            assert_eq!(done.status, InstanceStatus::Failed);
            assert_eq!(tasks(&run).len(), 2, "one correction round by default");
            assert_eq!(verifier.shown.lock().unwrap().len(), 2);
            assert_eq!(
                failure(&done),
                "Step 'code' was rejected by its verifier:\n\
                 - count matches the schema: the schema has three tables\n"
            );
            assert!(
                done.variables["steps"].get("code").is_none(),
                "a rejected package never becomes a step output"
            );
        }

        #[tokio::test]
        async fn a_verdict_that_cannot_be_read_fails_the_step_without_a_correction_round() {
            let (run, _) = verified_run(
                "true",
                &[THREE],
                vec![crate::handoff::Verdict::Unreadable(
                    "the verdict named no check".into(),
                )],
            )
            .await;
            let id = run.saved();

            let done = run.pick_up(&id).await;

            assert_eq!(done.status, InstanceStatus::Failed);
            assert_eq!(tasks(&run).len(), 1);
            assert!(failure(&done).starts_with("Step 'code' could not be verified"));
        }

        #[tokio::test]
        async fn a_step_leaving_by_a_declared_branch_is_not_judged_against_its_outputs() {
            let (run, verifier) = verified_run(
                "true\nbranches = [{ when = \"nothing-to-count\", goto = \"code\" }]",
                &["No tables here.\nOUTCOME: nothing-to-count"],
                Vec::new(),
            )
            .await;
            let id = run.saved();

            let done = run.pick_up(&id).await;

            assert_eq!(done.status, InstanceStatus::Exited);
            assert!(verifier.shown.lock().unwrap().is_empty());
            assert_eq!(tasks(&run).len(), 1);
        }

        #[tokio::test]
        async fn a_step_that_declines_verification_is_not_verified() {
            let (run, verifier) = verified_run("false", &[THREE], Vec::new()).await;
            let id = run.saved();

            let done = run.pick_up(&id).await;

            assert_eq!(done.status, InstanceStatus::Exited);
            assert!(verifier.shown.lock().unwrap().is_empty());
        }

        #[tokio::test]
        async fn a_round_that_fails_because_its_run_was_paused_does_not_fail_the_run() {
            let failing = Rounds {
                failing: true,
                ..Default::default()
            };
            let run = run_with(false, failing).await;
            let id = run.saved();
            run.mid_round(&id, |runtime, id| {
                let mut paused = runtime.load_instance(id).unwrap();
                paused.status = InstanceStatus::Paused;
                runtime.save_instance(&paused).unwrap();
            });

            run.runtime
                .execute_loop(&id, None, None, None)
                .await
                .unwrap();

            let after = run.runtime.load_instance(&id).unwrap();
            assert_eq!(after.status, InstanceStatus::Paused);
            assert!(note(&after).is_none(), "the step runs anew once resumed");
            assert!(after.step_history.is_empty());
        }

        #[tokio::test]
        async fn a_round_records_no_further_job_once_its_run_has_moved_on() {
            let run = run(false).await;
            let key = RoundKey::new();
            let id = run.interrupted(&key, None);
            let mut paused = run.runtime.load_instance(&id).unwrap();
            let noted = serde_json::from_value(note(&paused).unwrap()).unwrap();
            let journal = coding_round::StepJournal::new(Arc::clone(&run.runtime), &id, noted);
            paused.status = InstanceStatus::Paused;
            run.runtime.save_instance(&paused).unwrap();

            let next = key.jobs(false).job().to_string();
            assert!(journal.checkpoint("call", &next).await.is_err());
            let after = run.runtime.load_instance(&id).unwrap();
            assert_eq!(note(&after).unwrap()["checkpoint"], serde_json::Value::Null);
        }

        #[tokio::test]
        async fn aborting_a_run_ends_the_job_its_step_reached() {
            let run = run(false).await;
            let key = RoundKey::new();
            let mark = key.jobs(false).job().to_string();
            let id = run.interrupted(&key, Some(&mark));

            run.runtime
                .abort_workflow(&id, &Steer::hub())
                .await
                .unwrap();

            assert_eq!(
                *run.seats.ended.lock().unwrap(),
                vec![(format!("wf-{id}"), mark)]
            );
        }

        #[tokio::test]
        async fn going_to_a_step_ends_its_noted_round_so_that_it_runs_anew() {
            let run = run(false).await;
            let key = RoundKey::new();
            let mark = key.jobs(false).job().to_string();
            let id = run.interrupted(&key, Some(&mark));

            let kept = crate::decision_records::testing::record_in_memory();
            let alice = workforce_orchestrator::workflow::instance::Actor::Person {
                id: "alice-identity".into(),
            };
            let stood_at = run.runtime.load_instance(&id).unwrap().current_step_id;
            run.runtime
                .goto_step(&id, "code", &Steer::by(alice.clone()))
                .await
                .unwrap();

            assert_eq!(
                *run.seats.ended.lock().unwrap(),
                vec![(format!("wf-{id}"), mark)]
            );
            let moved = run.runtime.load_instance(&id).unwrap();
            assert!(note(&moved).is_none());
            let kept = kept.interventions_of(&id, 1).await;
            assert_eq!(kept.len(), 1, "the records say who moved it");
            assert_eq!(
                (kept[0].kind, &kept[0].actor),
                (InterventionKind::Goto, &alice)
            );
            assert_eq!(kept[0].from_step, stood_at);
            assert_eq!(kept[0].to_step.as_deref(), Some("code"));
            let stored = run.runtime.get_instance_status(&id).unwrap().unwrap();
            for part in [
                &stored.variables,
                &stored.step_history,
                &stored.trigger_json,
            ] {
                assert!(
                    !part.contains("alice-identity"),
                    "and the stored run does not: {part}"
                );
            }
        }

        /// A run whose history a newer build wrote — here a record naming
        /// who chose its route in words this build does not know — is never
        /// read as having none: its loop stops without saving, and the
        /// stored history and status stay exactly as they were.
        #[tokio::test]
        async fn a_run_whose_history_this_build_cannot_read_is_left_exactly_as_it_was() {
            let run = run(false).await;
            let id = run.saved();
            let store = &run.runtime.telemetry_store;
            let mut row = store.get_workflow_instance(&id).unwrap().unwrap();
            row.step_history = r#"[{"step_id":"code","outcome":"done","agent_response":"Done.","started_at":"2026-10-01T10:00:00Z","completed_at":"2026-10-01T10:01:00Z","rounds":1,"cost_usd":0.5,"outcome_by":"a_way_not_yet_invented"}]"#.into();
            store.save_workflow_instance(&row).unwrap();

            run.runtime
                .execute_loop(&id, None, None, None)
                .await
                .expect("the loop stops quietly");

            let after = store.get_workflow_instance(&id).unwrap().unwrap();
            assert_eq!(after.step_history, row.step_history, "not a byte changed");
            assert_eq!(after.status, row.status);
            assert_eq!(after.current_step_id, row.current_step_id);
            assert!(run.engine.runs().is_empty(), "no step ran");
            let err = format!("{:#}", run.runtime.load_instance(&id).unwrap_err());
            assert!(err.contains("cannot read"), "{err}");
            assert!(
                !err.contains("a_way_not_yet_invented"),
                "nothing the history holds: {err}"
            );
        }

        mod decide_first {
            use super::*;
            use crate::decide::testing::{
                backend, config_for, recording_backend, recording_backend_with_bodies, Requests,
            };
            use crate::decision_records::{InMemoryDecisionRecords, Record, Recorder};
            use workforce_common::settings::{DecisionBackend, DecisionConfig, DecisionUses};
            use workforce_decide::first;
            use workforce_orchestrator::workflow::engine::parse_outcome;
            use workforce_orchestrator::workflow::instance::{
                DecidedFirst, StepRecord, DECISION_ENGINE,
            };

            /// A ticket triage: `classify` is an agent step that sends a bug
            /// to `fix` and everything else on to `done`, and may be settled
            /// first. `{block}` adds to its `decide_first`.
            fn triage(block: &str) -> String {
                format!(
                    "[workflow]\nid = \"build\"\nname = \"Triage\"\n\n\
                     [[step]]\nid = \"classify\"\nname = \"Classify\"\n\
                     task = \"\"\"\nClassify the ticket.\nA crash or a wrong result: OUTCOME: bug\n\"\"\"\n\
                     engine = \"claude-code\"\n\
                     outputs = [{{ name = \"category\" }}, {{ name = \"reason\" }}]\n\
                     branches = [{{ when = \"bug\", goto = \"fix\" }}]\n\
                     goto = \"done\"\n\n\
                     [step.decide_first]\n\
                     state = '{{ \"subject\": \"{{{{subject}}}}\" }}'\n\
                     options = {{ ignore = \"A newsletter, an auto-reply or spam\" }}\n\
                     outputs = {{ category = \"{{{{decision.label}}}}\", reason = \"Settled by the decision model ({{{{decision.backend}}}}) at {{{{decision.probability}}}}\" }}\n\
                     {block}\n\
                     [[step]]\nid = \"fix\"\nname = \"Fix\"\ntask = \"Fix it.\"\n\
                     engine = \"claude-code\"\nexit = true\n\n\
                     [[step]]\nid = \"done\"\nname = \"Done\"\nexit = true\n\
                     report_render = \"template\"\n\
                     report = \"Closed as {{{{ steps.classify.category }}}}: {{{{ steps.classify.reason }}}}\"\n"
                )
            }

            const IGNORED: &str =
                "Spam.\nOUTPUTS:\ncategory: spam\nreason: a newsletter\nEND_OUTPUTS";
            const A_BUG: &str =
                "It crashes.\nOUTPUTS:\ncategory: bug\nreason: a crash\nEND_OUTPUTS\nOUTCOME: bug";

            struct Triage {
                run: Run,
                asked: Requests,
                kept: Arc<InMemoryDecisionRecords>,
            }

            fn uses(first: &str) -> DecisionUses {
                DecisionUses {
                    decide_first: toml::from_str(first).unwrap(),
                    ..Default::default()
                }
            }

            /// A triage whose decision model answers `choice` at 0.9 after
            /// `delay`, with `[decision.uses.decide_first]` set to `first`.
            async fn triage_run(
                block: &str,
                first: &str,
                choice: &'static str,
                delay: Duration,
                answers: &[&'static str],
            ) -> Triage {
                let (url, asked) = recording_backend(choice, delay).await;
                let config = DecisionConfig {
                    uses: uses(first),
                    backends: vec![DecisionBackend {
                        url,
                        max_concurrent: 1,
                        ..backend("kev")
                    }],
                    ..config_for(String::new())
                };
                configured(&triage(block), config, asked, answers).await
            }

            async fn configured(
                workflow: &str,
                config: DecisionConfig,
                asked: Requests,
                answers: &[&'static str],
            ) -> Triage {
                // Boxed: a runtime's set-up is a large future, and these
                // helpers would otherwise nest it by value in every test.
                let run = Box::pin(run_def(workflow, Rounds::answering(answers))).await;
                let kept = Arc::new(InMemoryDecisionRecords::default());
                run.runtime.set_decisions(
                    Arc::new(crate::decide::build(&config).unwrap().unwrap()),
                    Some(Recorder::start(kept.clone())),
                );
                Triage { run, asked, kept }
            }

            impl Triage {
                fn ticket(&self) -> String {
                    let trigger = TriggerInfo {
                        source: TriggerSource::Api,
                        payload: serde_json::json!({ "subject": "Weekly digest: 5 new posts" }),
                    };
                    let instance = WorkflowExecutor::start(&self.run.def, trigger, None).unwrap();
                    self.run.runtime.save_instance(&instance).unwrap();
                    instance.id
                }

                async fn triaged(&self) -> WorkflowInstance {
                    Box::pin(self.run.pick_up(&self.ticket())).await
                }

                async fn records(&self, expected: usize) -> Vec<Record> {
                    self.kept.settled(expected).await
                }
            }

            fn by(record: &StepRecord) -> (Option<&str>, Option<OutcomeBy>) {
                (record.outcome.as_deref(), record.provenance.outcome_by)
            }

            const ON: &str = "mode = \"on\"";

            #[tokio::test]
            async fn a_confident_answer_settles_the_step_and_its_agent_never_runs() {
                let triage = triage_run("", ON, "ignore", Duration::ZERO, &[]).await;

                let done = triage.triaged().await;

                assert_eq!(done.status, InstanceStatus::Completed);
                assert!(tasks(&triage.run).is_empty(), "no turn ran");
                let steps: Vec<&str> = done
                    .step_history
                    .iter()
                    .map(|r| r.step_id.as_str())
                    .collect();
                assert_eq!(steps, ["classify", "done"], "the default route");
                let classify = &done.step_history[0];
                assert_eq!(
                    by(classify),
                    (Some("ignore"), Some(OutcomeBy::DecisionModel))
                );
                assert!(classify.provenance.settled_first());
                assert_eq!(
                    classify.outputs,
                    Some(serde_json::json!({
                        "category": "ignore",
                        "reason": "Settled by the decision model (kev) at 0.90",
                    }))
                );
                assert!(!classify.failed && classify.correction.is_none());
                assert_eq!(parse_outcome(&classify.agent_response), None);
                assert_eq!(
                    done.step_history[1].report.as_deref(),
                    Some("Closed as ignore: Settled by the decision model (kev) at 0.90"),
                    "a later step reads its outputs like any completed step's"
                );
                assert_eq!(triage.asked.lock().len(), 1);
                assert_eq!(
                    triage.asked.lock()[0]["state"],
                    serde_json::json!({ "subject": "Weekly digest: 5 new posts" })
                );
            }

            #[tokio::test]
            async fn a_settled_step_records_who_settled_it_and_that_no_turn_was_paid_for() {
                let triage = triage_run("", ON, "ignore", Duration::ZERO, &[]).await;

                let done = triage.triaged().await;

                let classify = &done.step_history[0];
                assert_eq!(classify.provenance.engine.as_deref(), Some(DECISION_ENGINE));
                assert_eq!(classify.provenance.model.as_deref(), Some("kev"));
                assert_eq!(
                    classify.provenance.decided_first,
                    Some(DecidedFirst {
                        settled: true,
                        answer: "kev ignore p=0.90".into()
                    })
                );
                assert_eq!(classify.provenance.branch_decided_by, None);
                assert_eq!((classify.rounds, classify.cost_usd), (0, 0.0));
                assert_eq!(classify.provenance.input_tokens, None);
                assert!(
                    (classify.completed_at - classify.started_at).num_seconds() < 5,
                    "it took the ask, not a turn"
                );
                assert_eq!(done.total_cost(), 0.0);

                let records = triage.records(1).await;
                assert_eq!(records.len(), 1);
                let record = &records[0].record;
                assert_eq!(record.usage, "decide_first");
                assert_eq!(record.instance_id.as_deref(), Some(done.id.as_str()));
                assert_eq!(record.step_id.as_deref(), Some("classify"));
                assert!(record.acted);
                assert_eq!(record.verdict, None, "the model's own pick is no label");
                assert_eq!(record.answers[first::QUESTION_ID].verdict(), "ignore");
            }

            #[tokio::test]
            async fn an_answer_that_settles_on_a_branch_takes_that_branch() {
                let triage = triage_run("", ON, "bug", Duration::ZERO, &["Fixed."]).await;

                let done = triage.triaged().await;

                assert_eq!(done.status, InstanceStatus::Completed);
                assert_eq!(
                    by(&done.step_history[0]),
                    (Some("bug"), Some(OutcomeBy::DecisionModel))
                );
                assert_eq!(done.step_history[1].step_id, "fix");
                assert_eq!(tasks(&triage.run).len(), 1, "only the fix ran a turn");
            }

            #[tokio::test]
            async fn an_answer_below_its_floor_leaves_the_step_to_its_agent() {
                let triage = triage_run(
                    "min_probability = 0.95\n",
                    ON,
                    "ignore",
                    Duration::ZERO,
                    &[IGNORED],
                )
                .await;

                let done = triage.triaged().await;

                assert_eq!(tasks(&triage.run).len(), 1, "the turn ran");
                let classify = &done.step_history[0];
                assert_eq!(by(classify), (None, Some(OutcomeBy::Default)));
                assert_eq!(classify.outputs.as_ref().unwrap()["category"], "spam");
                assert_ne!(classify.provenance.engine.as_deref(), Some(DECISION_ENGINE));
                assert_eq!(classify.provenance.decided_first, None);
                let records = triage.records(1).await;
                assert!(!records[0].record.acted);
                assert_eq!(
                    records[0].record.verdict.as_deref(),
                    Some("ignore"),
                    "what the agent chose is the label"
                );
            }

            #[tokio::test]
            async fn a_step_that_withholds_the_escape_is_asked_its_options_alone_and_still_falls_through_below_its_floor(
            ) {
                for (block, settles) in [
                    ("escape = false\n", true),
                    ("escape = false\nmin_probability = 0.95\n", false),
                ] {
                    let triage = triage_run(block, ON, "ignore", Duration::ZERO, &[IGNORED]).await;

                    let done = triage.triaged().await;

                    let criteria =
                        triage.asked.lock()[0]["questions"][first::QUESTION_ID]["criteria"].clone();
                    let offered: Vec<&String> = criteria.as_object().unwrap().keys().collect();
                    assert_eq!(offered, ["bug", "ignore"], "{block:?}");
                    assert_eq!(
                        done.step_history[0].provenance.settled_first(),
                        settles,
                        "{block:?}"
                    );
                    assert_eq!(tasks(&triage.run).len(), usize::from(!settles), "{block:?}");
                }
            }

            #[tokio::test]
            async fn a_never_option_and_the_escape_answer_leave_the_step_to_its_agent() {
                for (block, choice) in [
                    ("floors = { bug = \"never\" }\n", "bug"),
                    ("", "an-option-nobody-offered"),
                    ("", workforce_decide::branch::NONE_OF_THESE),
                ] {
                    let triage =
                        triage_run(block, ON, choice, Duration::ZERO, &[A_BUG, "Fixed."]).await;

                    let done = triage.triaged().await;

                    assert_eq!(
                        tasks(&triage.run).len(),
                        2,
                        "{choice}: classify and fix ran"
                    );
                    assert_eq!(
                        by(&done.step_history[0]),
                        (Some("bug"), Some(OutcomeBy::OutcomeLine)),
                        "{choice}"
                    );
                    let records = triage.records(1).await;
                    assert_eq!(
                        records[0].record.verdict.as_deref(),
                        Some("bug"),
                        "{choice}"
                    );
                    assert!(!records[0].record.acted, "{choice}");
                }
            }

            #[tokio::test]
            async fn a_slow_backend_and_then_a_busy_one_cost_the_budget_and_the_agent_runs() {
                let triage = triage_run(
                    "",
                    "mode = \"on\"\nbudget_ms = 100",
                    "ignore",
                    Duration::from_secs(30),
                    &[IGNORED, IGNORED],
                )
                .await;

                let started = std::time::Instant::now();
                let timed_out = triage.triaged().await;
                let busy = triage.triaged().await;

                assert!(
                    started.elapsed() < Duration::from_secs(15),
                    "{:?}",
                    started.elapsed()
                );
                for done in [&timed_out, &busy] {
                    assert_eq!(done.status, InstanceStatus::Completed);
                    assert_eq!(by(&done.step_history[0]), (None, Some(OutcomeBy::Default)));
                }
                assert_eq!(tasks(&triage.run).len(), 2, "each run's turn ran");
                assert_eq!(
                    triage.asked.lock().len(),
                    1,
                    "the second ask found the one slot held and sent nothing"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert!(triage.kept.all().is_empty(), "no answer, no record");
            }

            #[tokio::test]
            async fn a_person_with_no_key_and_a_use_switched_off_are_never_asked() {
                let (url, asked) = recording_backend("ignore", Duration::ZERO).await;
                let no_key = DecisionConfig {
                    enabled: true,
                    backends: vec![DecisionBackend {
                        url,
                        ..backend("jev")
                    }],
                    primary: Some("jev".into()),
                    uses: uses(ON),
                    ..Default::default()
                };
                let no_key = configured(&triage(""), no_key, asked, &[IGNORED]).await;
                let done = no_key.triaged().await;
                assert_eq!(done.status, InstanceStatus::Completed);
                assert_eq!(tasks(&no_key.run).len(), 1, "the turn ran");
                assert!(
                    no_key.asked.lock().is_empty(),
                    "nothing was sent without a key"
                );

                for (block, first) in [("", "mode = \"off\""), ("mode = \"off\"\n", ON)] {
                    let off = triage_run(block, first, "ignore", Duration::ZERO, &[IGNORED]).await;
                    let done = off.triaged().await;
                    assert_eq!(by(&done.step_history[0]), (None, Some(OutcomeBy::Default)));
                    assert_eq!(tasks(&off.run).len(), 1, "{first} {block}");
                    assert!(off.asked.lock().is_empty(), "{first} {block}");
                }
            }

            /// The ask runs as the person the run runs as, with the step
            /// agent's vaults: their own key is used, and someone else's
            /// never is.
            #[tokio::test]
            async fn the_ask_uses_the_run_as_persons_own_key_and_nobody_elses() {
                for (holder, asked) in [("alice", 1), ("bob", 0)] {
                    let (url, requests) = recording_backend("ignore", Duration::ZERO).await;
                    let keyed = DecisionConfig {
                        enabled: true,
                        backends: vec![DecisionBackend {
                            url,
                            ..backend("jev")
                        }],
                        primary: Some("jev".into()),
                        uses: uses(ON),
                        ..Default::default()
                    };
                    let triage = configured(&triage(""), keyed, requests, &[IGNORED]).await;
                    let (service, _) = triage.run.runtime.resolve_step_service(None).unwrap();
                    service
                        .vault
                        .as_ref()
                        .expect("the step agent's vault")
                        .set(&format!("users/{holder}/decision/jev/api_key"), "a-key")
                        .unwrap();
                    let trigger = TriggerInfo {
                        source: TriggerSource::Api,
                        payload: serde_json::json!({ "subject": "Weekly digest" }),
                    };
                    let mut instance =
                        WorkflowExecutor::start(&triage.run.def, trigger, None).unwrap();
                    instance.variables["_run_as"] = serde_json::json!("alice");
                    triage.run.runtime.save_instance(&instance).unwrap();

                    let done = Box::pin(triage.run.pick_up(&instance.id)).await;

                    assert_eq!(triage.asked.lock().len(), asked, "{holder}'s key");
                    assert_eq!(tasks(&triage.run).len(), 1 - asked, "{holder}'s key");
                    assert_eq!(done.status, InstanceStatus::Completed);
                }
            }

            #[tokio::test]
            async fn shadow_runs_the_agent_and_records_whether_it_agreed() {
                for (block, mode) in [("", "mode = \"shadow\""), ("mode = \"shadow\"\n", ON)] {
                    for (answer, agreed, chose) in [
                        (IGNORED, "agent agreed", "ignore"),
                        (A_BUG, "agent chose bug", "bug"),
                    ] {
                        let triage =
                            triage_run(block, mode, "ignore", Duration::ZERO, &[answer, "Fixed."])
                                .await;

                        let done = triage.triaged().await;

                        let classify = &done.step_history[0];
                        assert!(!tasks(&triage.run).is_empty(), "the turn ran");
                        assert_ne!(
                            classify.provenance.outcome_by,
                            Some(OutcomeBy::DecisionModel)
                        );
                        assert_eq!(
                            classify.provenance.decided_first,
                            Some(DecidedFirst {
                                settled: false,
                                answer: format!("kev (shadow) ignore p=0.90, {agreed}")
                            })
                        );
                        let records = triage.records(1).await;
                        assert_eq!(records.len(), 1);
                        assert!(!records[0].record.acted);
                        assert_eq!(records[0].record.verdict.as_deref(), Some(chose));
                        assert_eq!(
                            crate::decide::would_settle(
                                &records[0].record.input,
                                &records[0].record.answers
                            ),
                            Some(("ignore".to_string(), 0.9)),
                            "the record says the model would have settled it"
                        );
                    }
                }
            }

            /// What a step was sent before its turn, and the record the
            /// exporter writes for the example gathered from what was kept of
            /// that ask, are the same bytes, labelled with what the agent's
            /// turn chose — in shadow and below the floor. A step the model
            /// settled ran no turn and teaches nothing.
            #[tokio::test]
            async fn a_gathered_decide_first_example_is_byte_for_byte_the_request_the_step_sent() {
                use workforce_decide::export::{self, LeftOut};
                let long = "Unsubscribe from these emails at any time. ".repeat(80);
                for (block, chose, label) in [
                    ("mode = \"shadow\"\n", IGNORED, Some("ignore")),
                    ("min_probability = 0.95\n", A_BUG, Some("bug")),
                    (
                        "mode = \"shadow\"\nescape = false\n",
                        IGNORED,
                        Some("ignore"),
                    ),
                    ("", IGNORED, None),
                ] {
                    let workflow = triage(block).replace(
                        "{{subject}}\" }'",
                        &format!("{{{{subject}}}}\", \"body\": \"{long}\" }}'"),
                    );
                    let (url, asked, bodies) =
                        recording_backend_with_bodies("ignore", Duration::ZERO).await;
                    let config = DecisionConfig {
                        uses: uses("mode = \"on\"\nstate_chars = 600"),
                        ..config_for(url)
                    };
                    let triage = configured(&workflow, config, asked, &[chose, "Fixed."]).await;
                    assert!(triage.run.def.steps[0]
                        .decide_first
                        .as_ref()
                        .unwrap()
                        .state
                        .contains("body"));
                    triage.triaged().await;

                    let records = triage.records(1).await;
                    let example = crate::decide_gather::decide_first_example(&records[0]);
                    assert_eq!(example.label.as_str(), label, "{block:?}");
                    let line = export::line(
                        &serde_json::to_string(&example).unwrap(),
                        workforce_decide::RequestBudget::chars(records[0].record.request_chars),
                    )
                    .unwrap();
                    let Ok(record) = line else {
                        assert_eq!(line, Err(LeftOut::Unlabelled), "{block:?}");
                        assert!(records[0].record.acted, "settled: the model's own pick");
                        continue;
                    };
                    let exported: serde_json::Value = serde_json::from_str(&record).unwrap();
                    let body = bodies.lock()[0].clone();
                    for field in ["state", "questions"] {
                        let span = format!("\"{field}\":{}", exported[field]);
                        assert!(
                            body.contains(&span) && record.contains(&span),
                            "{block:?}: {field} is not the same bytes on the wire as in the record:\n{body}\n{record}"
                        );
                    }
                    let sent = triage.asked.lock()[0].clone();
                    assert!(
                        sent["state"]["body"].as_str().unwrap().chars().count() < long.len(),
                        "the state was held to the use's budget"
                    );
                    assert_eq!(exported["source"], "agent");
                }
            }

            #[tokio::test]
            async fn a_turn_sent_back_for_correction_is_finished_by_its_agent_without_asking_again()
            {
                let triage = triage_run(
                    "min_probability = 0.95\n",
                    ON,
                    "ignore",
                    Duration::ZERO,
                    &["Spam, I think.", IGNORED],
                )
                .await;

                let done = triage.triaged().await;

                assert_eq!(done.status, InstanceStatus::Completed);
                assert_eq!(tasks(&triage.run).len(), 2, "the turn and its correction");
                assert!(done.step_history[0].correction.is_some());
                assert_eq!(
                    triage.asked.lock().len(),
                    1,
                    "asked once, before the first turn"
                );
                let records = triage.records(1).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert_eq!(triage.kept.all().len(), 1);
                assert_eq!(
                    records[0].record.verdict, None,
                    "the turn it was asked beside was sent back; its record is kept unlabelled"
                );
            }

            #[tokio::test]
            async fn a_settled_step_whose_outputs_do_not_read_is_left_to_its_agent() {
                let typed = triage("").replace(
                    "{ name = \"category\" }",
                    "{ name = \"category\", type = \"integer\" }",
                );
                let (url, asked) = recording_backend("ignore", Duration::ZERO).await;
                let config = DecisionConfig {
                    uses: uses(ON),
                    ..config_for(url)
                };
                let agent = ["Spam.\nOUTPUTS:\ncategory: 7\nreason: spam\nEND_OUTPUTS"];
                let triage = configured(&typed, config, asked, &agent).await;

                let done = triage.triaged().await;

                assert_eq!(
                    done.status,
                    InstanceStatus::Completed,
                    "the run is not failed"
                );
                assert_eq!(tasks(&triage.run).len(), 1, "the turn ran");
                let classify = &done.step_history[0];
                assert_eq!(classify.outputs.as_ref().unwrap()["category"], 7);
                assert_eq!(
                    classify.provenance.decided_first,
                    Some(DecidedFirst {
                        settled: false,
                        answer: "kev (not settled) ignore p=0.90, agent agreed".into()
                    })
                );
            }

            #[tokio::test]
            async fn a_run_interrupted_after_the_ask_settles_once_and_a_settled_one_is_never_asked_again(
            ) {
                let triage = triage_run("", ON, "ignore", Duration::ZERO, &[]).await;
                let id = triage.ticket();

                // A loop that asked and died before its save left nothing behind.
                let instance = triage.run.runtime.load_instance(&id).unwrap();
                let exec = WorkflowExecutor::prepare_step(&triage.run.def, &instance).unwrap();
                let (service, _) = triage.run.runtime.resolve_step_service(None).unwrap();
                let lost = triage
                    .run
                    .runtime
                    .ask_first(
                        &instance,
                        triage.run.def.get_step("classify"),
                        &exec,
                        &service,
                    )
                    .await
                    .expect("an answer");
                assert!(lost.settled().is_some());
                drop(lost);
                assert!(triage
                    .run
                    .runtime
                    .load_instance(&id)
                    .unwrap()
                    .step_history
                    .is_empty());

                let done = triage.run.pick_up(&id).await;
                let classified = |run: &WorkflowInstance| {
                    run.step_history
                        .iter()
                        .filter(|r| r.step_id == "classify")
                        .count()
                };
                assert_eq!(classified(&done), 1, "settled once");
                assert_eq!(
                    triage.asked.lock().len(),
                    2,
                    "asked again by the loop that took over"
                );

                let again = triage.run.pick_up(&id).await;
                assert_eq!(classified(&again), 1);
                assert_eq!(triage.asked.lock().len(), 2, "a finished run asks nothing");
                assert!(tasks(&triage.run).is_empty(), "and no turn ever ran");
                let records = triage.records(2).await;
                let mut kept: Vec<(bool, Option<String>)> = records
                    .iter()
                    .map(|r| (r.record.acted, r.record.verdict.clone()))
                    .collect();
                kept.sort();
                assert_eq!(
                    kept,
                    [(false, None), (true, None)],
                    "each ask is recorded, unlabelled, and only the saved settle as acted on"
                );
            }

            #[tokio::test]
            async fn a_replica_that_lost_the_run_mid_ask_cannot_save_its_settle() {
                let triage = triage_run(
                    "",
                    "mode = \"on\"\nbudget_ms = 30000",
                    "ignore",
                    Duration::from_secs(2),
                    &[],
                )
                .await;
                let id = triage.ticket();
                let runtime = Arc::clone(&triage.run.runtime);
                let run_id = id.clone();
                let looping =
                    tokio::spawn(
                        async move { runtime.execute_loop(&run_id, None, None, None).await },
                    );

                // While the model is being asked, another replica takes the run.
                tokio::time::timeout(Duration::from_secs(20), async {
                    while triage.asked.lock().is_empty() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("the ask is in flight");
                let store = &triage.run.runtime.telemetry_store;
                store
                    .release_workflow_instance(&id, process_instance_id())
                    .unwrap();
                assert!(store
                    .try_claim_workflow_instance(&id, "another-hub", 120)
                    .unwrap());

                looping.await.unwrap().unwrap();
                let stored = triage.run.runtime.load_instance(&id).unwrap();
                assert!(
                    stored.step_history.is_empty(),
                    "the settle was fenced out: the run is the other replica's to ask and run"
                );
                assert_eq!(stored.status, InstanceStatus::Running);
                assert!(tasks(&triage.run).is_empty());
            }
        }
    }

    const UNMET: &str = "requires absent-skill >=0.10.0; not installed";

    /// The instance once its executor has finished with it.
    async fn settled(runtime: &WorkflowRuntime, id: &str) -> WorkflowInstance {
        for _ in 0..500 {
            let instance = runtime.load_instance(id).unwrap();
            if instance.is_terminal() && !runtime.active_loops.lock().contains(id) {
                return instance;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("run {id} never finished");
    }

    #[tokio::test]
    async fn a_run_started_by_background_work_keeps_whom_it_bills() {
        const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
        let runtime = test_runtime().await;
        runtime
            .definitions
            .write()
            .await
            .insert("smoke".into(), owned(requiring_def(1), None));
        let principal = TurnPrincipal {
            run_as: Some("heartbeat".into()),
            bill_to: Some(ALICE.into()),
        };
        let id = crate::user_config::as_principal(
            &principal,
            runtime.start_workflow("smoke", serde_json::json!({}), None, None, None, None),
        )
        .await
        .unwrap();
        assert_eq!(
            instance_principal(&settled(&runtime, &id).await),
            principal,
            "every later entry into the run bills its steps the same way"
        );
    }

    #[tokio::test]
    async fn a_run_whose_required_skill_is_not_installed_fails_before_its_first_step() {
        let runtime = test_runtime().await;
        runtime
            .definitions
            .write()
            .await
            .insert("smoke".into(), owned(requiring_def(1), None));
        let id = runtime
            .start_workflow("smoke", serde_json::json!({}), None, None, None, None)
            .await
            .unwrap();
        let failed = settled(&runtime, &id).await;
        assert_eq!(failed.status, InstanceStatus::Failed);
        assert!(failed.step_history.is_empty(), "no step ran");
        assert_eq!(failed.variables["_failure_reason"], UNMET);
    }

    #[tokio::test]
    async fn a_queued_run_is_checked_for_its_required_skills_when_promoted() {
        let (runtime, occupant) = runtime_with_occupant(1).await;
        let queued = trigger_start(&runtime).await;
        assert_eq!(status_of(&runtime, &queued), InstanceStatus::Queued);
        runtime
            .definitions
            .write()
            .await
            .insert("smoke".into(), owned(requiring_def(1), None));

        let mut done = runtime.load_instance(&occupant).unwrap();
        done.status = InstanceStatus::Completed;
        runtime.save_instance(&done).unwrap();
        runtime.promote_queued("smoke").await;

        let failed = settled(&runtime, &queued).await;
        assert_eq!(
            (failed.status, failed.step_history.len()),
            (InstanceStatus::Failed, 0)
        );
        assert_eq!(failed.variables["_failure_reason"], UNMET);
    }

    #[tokio::test]
    async fn saving_a_scheduled_single_slot_decision_without_expiry_warns_that_runs_are_skipped() {
        let runtime = test_runtime().await;
        let toml = |expiry: &str| {
            format!(
                "[workflow]\nid = \"daily-digest-email\"\nname = \"Digest\"\n\n\
                 [[trigger]]\ntype = \"schedule\"\ncron = \"0 7 * * *\"\n\n\
                 [[step]]\nid = \"review\"\nname = \"Review\"\nreport = \"Today.\"\n\
                 report_render = \"template\"\npause = \"awaiting-approval\"\n\
                 branches = [{{ when = \"send\", goto = \"send\" }}]\n\n\
                 [step.decision]\ndeciders = \"approver.one@example.com\"\n{expiry}\n\n\
                 [[step.decision.buttons]]\nlabel = \"Send\"\noutcome = \"send\"\n\n\
                 [[step.decision.buttons]]\nlabel = \"Cancel\"\noutcome = \"cancel\"\nend = \"cancel\"\n\n\
                 [[step]]\nid = \"send\"\nname = \"Send\"\nreport = \"Sent.\"\n\
                 report_render = \"template\"\nexit = true\n"
            )
        };
        let save = |source: String| {
            let runtime = Arc::clone(&runtime);
            async move {
                runtime
                    .save_definition_source(
                        "daily-digest-email",
                        &source,
                        None,
                        Visibility::Global,
                        RunAsWrite::Unchanged,
                        workforce_orchestrator::workflow_store::EditContext::new("test", None),
                    )
                    .await
                    .unwrap()
            }
        };

        let unbounded = save(toml("")).await;
        assert_eq!(unbounded.len(), 1, "{unbounded:?}");
        assert!(
            unbounded[0].starts_with("saved, but step 'review' waits on a decision")
                && unbounded[0].contains("every later scheduled run is skipped"),
            "{unbounded:?}"
        );
        assert!(
            save(toml("expires_after = \"20h\"\non_expiry = \"cancel\""))
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn saving_a_definition_whose_required_skill_is_not_installed_saves_with_a_warning() {
        let runtime = test_runtime().await;
        let requires = "requires = { \"absent-skill\" = \">=0.10.0\" }\n";
        let toml = format!(
            "[workflow]\nid = \"needs-skill\"\nname = \"Needs a skill\"\n{requires}\n\
             [[step]]\nid = \"run\"\nname = \"R\"\ntask = \"go\"\nexit = true\n"
        );
        let save = |source: String| {
            let runtime = Arc::clone(&runtime);
            async move {
                runtime
                    .save_definition_source(
                        "needs-skill",
                        &source,
                        None,
                        Visibility::Global,
                        RunAsWrite::Unchanged,
                        workforce_orchestrator::workflow_store::EditContext::new("test", None),
                    )
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            save(toml.clone()).await,
            [format!(
                "saved, but its runs fail before their first step until this is met: {UNMET}"
            )]
        );
        assert_eq!(
            runtime
                .get_definition("needs-skill")
                .await
                .map(|def| def.workflow.requires.len()),
            Some(1),
            "the definition is saved all the same"
        );
        assert!(save(toml.replace(requires, "")).await.is_empty());
    }

    #[tokio::test]
    async fn promotion_is_a_no_op_while_the_cap_is_still_held() {
        let (runtime, occupant) = runtime_with_occupant(1).await;
        let queued = trigger_start(&runtime).await;
        runtime.promote_queued("smoke").await;
        assert_eq!(status_of(&runtime, &queued), InstanceStatus::Queued);
        assert_eq!(
            runtime.active_instance_ids("smoke").unwrap(),
            vec![occupant]
        );
    }

    /// The two operator actions whose guards were written as "not terminal"
    /// and so admitted a queued run: both would have parked it in `Paused`
    /// (or `Completed` on the last step), which counts against the cap the
    /// run was never admitted to *and* drops it out of the queue for good.
    #[tokio::test]
    async fn operator_step_actions_refuse_a_queued_instance() {
        let (runtime, _occupant) = runtime_with_occupant(1).await;
        let queued = trigger_start(&runtime).await;

        let goto = runtime.goto_step(&queued, "run", &Steer::hub()).await;
        assert!(goto.is_err(), "goto_step must refuse a queued instance");
        let skip = runtime.skip_step(&queued, &Steer::hub()).await;
        assert!(skip.is_err(), "skip_step must refuse a queued instance");
        for err in [goto.unwrap_err(), skip.unwrap_err()] {
            assert!(err.to_string().contains("queued"), "{err}");
        }

        assert_eq!(status_of(&runtime, &queued), InstanceStatus::Queued);
        assert!(!runtime
            .active_instance_ids("smoke")
            .unwrap()
            .contains(&queued));
    }

    /// Aborting the occupant frees its slot, so the queue drains without
    /// waiting for a maintenance tick.
    #[tokio::test]
    async fn aborting_the_occupant_promotes_a_queued_sibling() {
        let (runtime, occupant) = runtime_with_occupant(1).await;
        let queued = trigger_start(&runtime).await;
        assert_eq!(status_of(&runtime, &queued), InstanceStatus::Queued);

        runtime
            .abort_workflow(&occupant, &Steer::hub())
            .await
            .unwrap();

        assert_eq!(status_of(&runtime, &occupant), InstanceStatus::Cancelled);
        assert_eq!(status_of(&runtime, &queued), InstanceStatus::Running);
    }

    /// A queued instance is cancellable — it is reachable by the operator
    /// surfaces and leaves the queue without ever consuming a slot.
    #[tokio::test]
    async fn a_queued_instance_can_be_cancelled_without_taking_a_slot() {
        let (runtime, occupant) = runtime_with_occupant(1).await;
        let queued = trigger_start(&runtime).await;

        runtime
            .abort_workflow(&queued, &Steer::hub())
            .await
            .unwrap();

        assert_eq!(status_of(&runtime, &queued), InstanceStatus::Cancelled);
        assert_eq!(
            runtime.active_instance_ids("smoke").unwrap(),
            vec![occupant],
            "cancelling a queued run must not disturb the occupant"
        );
    }

    /// The definitions map for `defs`, keyed as the runtime keys it.
    fn def_map<const N: usize>(defs: [OwnedDefinition; N]) -> HashMap<String, OwnedDefinition> {
        defs.into_iter().map(|d| (d.key(), d)).collect()
    }

    /// The overlay a store would return for the given `(definition, trigger)`
    /// pairs.
    fn overlay(entries: &[(&OwnedDefinition, &str, &str)]) -> HashMap<TriggerBinding, String> {
        entries
            .iter()
            .map(|(owned, trigger_id, path)| {
                (trigger_binding(owned, trigger_id), (*path).to_string())
            })
            .collect()
    }

    fn routed(index: &TriggerIndex, path: &str) -> Option<String> {
        index.route(path).map(|r| r.def_key)
    }

    #[test]
    fn http_index_maps_unique_ids() {
        let defs = def_map([
            owned(def_with_http_trigger("a", "deploy"), None),
            owned(def_with_http_trigger("b", "notify"), None),
        ]);
        let index = TriggerIndex::build(&defs, &HashMap::new());
        assert_eq!(routed(&index, "deploy").as_deref(), Some("a"));
        assert_eq!(routed(&index, "notify").as_deref(), Some("b"));
    }

    #[test]
    fn http_index_drops_colliding_ids() {
        let defs = def_map([
            owned(def_with_http_trigger("a", "dup"), None),
            owned(def_with_http_trigger("b", "dup"), None),
        ]);
        let index = TriggerIndex::build(&defs, &HashMap::new());
        assert!(
            routed(&index, "dup").is_none(),
            "colliding id must be absent"
        );
    }

    #[test]
    fn http_index_owner_namespaces_member_trigger_paths() {
        // A member trigger is keyed by its owner-namespaced public path
        // (/triggers/{owner}/{id}), not the bare id — so two members' "deploy"
        // triggers don't collide. The value is the definition's addressing key.
        let defs = def_map([owned(def_with_http_trigger("wf", "deploy"), Some("alice"))]);
        let index = TriggerIndex::build(&defs, &HashMap::new());
        assert_eq!(routed(&index, "alice/deploy").as_deref(), Some("alice/wf"));
        assert!(
            routed(&index, "deploy").is_none(),
            "member trigger must not claim the bare path"
        );
    }

    #[test]
    fn http_index_two_members_same_trigger_id_no_collision() {
        let defs = def_map([
            owned(def_with_http_trigger("wf", "deploy"), Some("alice")),
            owned(def_with_http_trigger("wf", "deploy"), Some("bob")),
        ]);
        let index = TriggerIndex::build(&defs, &HashMap::new());
        assert_eq!(routed(&index, "alice/deploy").as_deref(), Some("alice/wf"));
        assert_eq!(routed(&index, "bob/deploy").as_deref(), Some("bob/wf"));
    }

    #[test]
    fn stable_and_legacy_paths_both_resolve_to_one_trigger() {
        let alice = owned(def_with_http_trigger("wf", "deploy"), Some("alice"));
        let endpoints = overlay(&[(&alice, "deploy", "t.abc")]);
        let index = TriggerIndex::build(&def_map([alice]), &endpoints);

        let expected = TriggerRoute {
            def_key: "alice/wf".to_string(),
            trigger_id: "deploy".to_string(),
        };
        assert_eq!(index.route("alice/deploy"), Some(expected.clone()));
        assert_eq!(index.route("t.abc"), Some(expected));
    }

    /// The compatibility guarantee in its smallest form: with nothing minted
    /// — a hub with no definition store, or one whose endpoint read failed —
    /// every address that resolved before this change still resolves.
    #[test]
    fn a_legacy_path_resolves_with_no_endpoint_overlay() {
        let defs = def_map([
            owned(def_with_http_trigger("sys", "deploy"), None),
            owned(def_with_http_trigger("wf", "deploy"), Some("alice")),
        ]);
        let index = TriggerIndex::build(&defs, &HashMap::new());

        assert_eq!(routed(&index, "deploy").as_deref(), Some("sys"));
        assert_eq!(routed(&index, "alice/deploy").as_deref(), Some("alice/wf"));
        assert_eq!(
            index.paths_for("alice/wf"),
            [TriggerPaths {
                trigger_id: "deploy".to_string(),
                stable: None,
                legacy: Some("alice/deploy".to_string()),
            }]
        );
    }

    /// The disjointness proof, executed against the real validator: a minted
    /// path can never be mistaken for an owner-derived one, in either
    /// direction, so one index holds both with no shadowing.
    #[test]
    fn a_minted_path_can_never_equal_a_legacy_path() {
        let minted = mint_trigger_path();
        assert!(minted.starts_with(TRIGGER_PATH_PREFIX));
        assert!(!minted.contains('/'));
        assert!(
            !is_valid_http_trigger_id(&minted),
            "a minted path must never validate as a trigger id"
        );
        assert!(!is_valid_http_trigger_id("has.dot"));

        // A system legacy path is a validated trigger id, so it has no `.`; a
        // member one is `{owner}/{id}`, so it always has a `/`.
        assert!(is_valid_http_trigger_id("deploy"));
        assert!(!trigger_public_id(None, "deploy").contains('.'));
        assert!(trigger_public_id(Some("alice"), "deploy").contains('/'));
    }

    /// Today two system definitions sharing a trigger id take both endpoints
    /// off the air. They still lose the shared bare path — fail closed on
    /// ambiguity — but each keeps its own minted one.
    #[test]
    fn a_legacy_collision_leaves_both_stable_paths_working() {
        let a = owned(def_with_http_trigger("a", "dup"), None);
        let b = owned(def_with_http_trigger("b", "dup"), None);
        let endpoints = overlay(&[(&a, "dup", "t.aaa"), (&b, "dup", "t.bbb")]);
        let index = TriggerIndex::build(&def_map([a, b]), &endpoints);

        assert!(routed(&index, "dup").is_none());
        assert_eq!(routed(&index, "t.aaa").as_deref(), Some("a"));
        assert_eq!(routed(&index, "t.bbb").as_deref(), Some("b"));
    }

    /// What a transfer does to the same definition: the minted path is
    /// unmoved by the owner, while the owner-derived one moves with them.
    #[test]
    fn a_stable_path_is_unaffected_by_the_owner() {
        for owner in ["alice", "bob"] {
            let def = owned(def_with_http_trigger("wf", "deploy"), Some(owner));
            let endpoints = overlay(&[(&def, "deploy", "t.abc")]);
            let index = TriggerIndex::build(&def_map([def]), &endpoints);

            let key = format!("{owner}/wf");
            assert_eq!(routed(&index, "t.abc").as_deref(), Some(key.as_str()));
            assert_eq!(
                routed(&index, &format!("{owner}/deploy")).as_deref(),
                Some(key.as_str())
            );
        }
    }

    /// The two halves of what a transfer does to an external caller, in one
    /// place so the consequence is visible in CI rather than in production.
    ///
    /// The minted address survives, carrying the same bytes to the same
    /// workflow under its new owner — which is why decoupling the trigger path
    /// from ownership shipped before transfer did. The owner-derived address
    /// *names* the old owner, so no indirection can make it go on meaning the
    /// same thing: it stops resolving, and the transfer response says so
    /// rather than leaving a 404 to be discovered.
    #[test]
    fn a_transfer_keeps_the_stable_trigger_address_and_drops_the_owner_derived_one() {
        let before = owned(def_with_http_trigger("wf", "deploy"), Some("alice"));
        let after = owned(def_with_http_trigger("wf", "deploy"), Some("bob"));
        // The endpoint row moves with the definition, `public_path` untouched,
        // so the address the store hands back afterwards is the same string.
        let minted = overlay(&[(&before, "deploy", "t.abc")]);
        let path = minted.values().next().expect("a minted path").clone();
        let index = TriggerIndex::build(
            &def_map([after.clone()]),
            &overlay(&[(&after, "deploy", &path)]),
        );

        assert_eq!(routed(&index, &path).as_deref(), Some("bob/wf"));
        assert_eq!(routed(&index, "alice/deploy"), None);
        assert_eq!(routed(&index, "bob/deploy").as_deref(), Some("bob/wf"));
    }

    /// A dropped legacy path must read as absent, not as a copyable URL that
    /// 404s — and a trigger with no minted path is still reported, so a
    /// surface can say why an address is missing rather than hiding it.
    #[test]
    fn paths_for_reports_every_trigger_including_the_dropped_ones() {
        let a = owned(def_with_http_trigger("a", "dup"), None);
        let b = owned(def_with_http_trigger("b", "dup"), None);
        let endpoints = overlay(&[(&a, "dup", "t.aaa")]);
        let index = TriggerIndex::build(&def_map([a, b]), &endpoints);

        assert_eq!(
            index.paths_for("a"),
            [TriggerPaths {
                trigger_id: "dup".to_string(),
                stable: Some("t.aaa".to_string()),
                legacy: None,
            }]
        );
        assert_eq!(
            index.paths_for("b"),
            [TriggerPaths {
                trigger_id: "dup".to_string(),
                stable: None,
                legacy: None,
            }]
        );
        assert!(index.paths_for("unknown").is_empty());
    }

    #[test]
    fn trigger_path_form_classifies_both_forms() {
        assert_eq!(trigger_path_form(&mint_trigger_path()), "stable");
        assert_eq!(trigger_path_form("deploy"), "legacy");
        assert_eq!(trigger_path_form("alice/deploy"), "legacy");
    }

    #[test]
    fn absolute_urls_use_public_url_and_fall_back_to_the_placeholder() {
        let paths = TriggerPaths {
            trigger_id: "deploy".to_string(),
            stable: Some("t.abc".to_string()),
            legacy: Some("alice/deploy".to_string()),
        };

        let configured = trigger_urls_of(public_base_url(Some("https://hub.example.com/")), &paths);
        assert_eq!(
            configured.url.as_deref(),
            Some("https://hub.example.com/triggers/t.abc")
        );
        assert_eq!(
            configured.legacy_url.as_deref(),
            Some("https://hub.example.com/triggers/alice/deploy")
        );

        let unset = trigger_urls_of(public_base_url(None), &paths);
        assert_eq!(
            unset.url.as_deref(),
            Some("https://<hub.public_url>/triggers/t.abc")
        );
    }

    /// A definition declaring the same trigger id twice is not rejected by
    /// `validate()`, so the index must collapse it rather than report the
    /// trigger twice or treat it as a collision with itself.
    #[test]
    fn a_repeated_trigger_id_within_one_definition_is_one_trigger() {
        let mut def = def_with_http_trigger("wf", "deploy");
        def.triggers.push(def.triggers[0].clone());
        let def = owned(def, Some("alice"));
        let endpoints = overlay(&[(&def, "deploy", "t.abc")]);
        let index = TriggerIndex::build(&def_map([def]), &endpoints);

        assert_eq!(index.paths_for("alice/wf").len(), 1);
        assert_eq!(routed(&index, "alice/deploy").as_deref(), Some("alice/wf"));
        assert_eq!(routed(&index, "t.abc").as_deref(), Some("alice/wf"));
    }

    #[test]
    fn compute_secret_policy_gates_per_user_and_declares() {
        use workforce_orchestrator::workflow::definition::VariableKind;
        let var = |id: &str, kind, value: &str, hosts: &[&str]| VariableDef {
            id: id.into(),
            kind,
            value: value.into(),
            allowed_hosts: hosts.iter().map(|h| h.to_string()).collect(),
        };
        let vars = vec![
            var("email", VariableKind::String, "a@b.com", &[]),
            var(
                "pw",
                VariableKind::Secret,
                "[[vault:global_user:portal_pw]]",
                &["x.example.com"],
            ),
            var(
                "api",
                VariableKind::Secret,
                "[[vault:agent:workflow_shared/key]]",
                &[],
            ),
        ];

        // Owner runs own → per-user enabled, curated prefix set, both secrets
        // declared (the plain var is excluded).
        let p = compute_secret_policy(
            Some("alice"),
            Some("alice".into()),
            &vars,
            "workflow_shared/",
        );
        assert_eq!(p.per_user_identity.as_deref(), Some("alice"));
        assert_eq!(p.shared_secret_prefix.as_deref(), Some("workflow_shared/"));
        assert_eq!(p.declared.len(), 2);
        assert_eq!(
            p.declared
                .get(&("global_user".into(), "portal_pw".into()))
                .unwrap()
                .allowed_hosts,
            vec!["x.example.com"]
        );

        // Non-owner runs a shared definition → no per-user identity.
        let p2 =
            compute_secret_policy(Some("alice"), Some("bob".into()), &vars, "workflow_shared/");
        assert!(p2.per_user_identity.is_none());

        // System definition → its run_as identity reads its own per-user secrets,
        // and shared references are unrestricted (no curated prefix).
        let p3 = compute_secret_policy(None, Some("ops".into()), &vars, "workflow_shared/");
        assert_eq!(p3.per_user_identity.as_deref(), Some("ops"));
        assert!(p3.shared_secret_prefix.is_none());
    }

    #[test]
    fn quota_counts_owner_prefix_without_collision() {
        let ids: Vec<String> = ["alice/a", "alice/b", "alice2/x", "bob/y", "syswf"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // alice has exactly 2 — `alice2` and the bare system id don't count.
        assert!(quota_exceeded(&ids, "alice", 2));
        assert!(!quota_exceeded(&ids, "alice", 3));
        assert!(!quota_exceeded(&ids, "bob", 2));
    }

    #[test]
    fn def_key_namespaces_member_definitions_only() {
        assert_eq!(def_key(None, "wf"), "wf");
        assert_eq!(def_key(Some("alice"), "wf"), "alice/wf");
    }

    #[test]
    fn execution_identity_derivation() {
        let bob = || Some("bob".to_string());
        // Interactive: the runner.
        assert_eq!(
            derive_execution_identity(Some("alice"), None, false, bob()),
            bob()
        );
        // Automated, member-owned: the owner, ignoring any trigger run_as …
        assert_eq!(
            derive_execution_identity(Some("alice"), None, true, bob()),
            Some("alice".into())
        );
        // … unless the definition names a run identity.
        assert_eq!(
            derive_execution_identity(Some("alice"), Some("carol"), true, bob()),
            Some("carol".into())
        );
        // Automated, system/disk: the explicit (operator-trusted) run_as.
        assert_eq!(
            derive_execution_identity(None, None, true, Some("ops".into())),
            Some("ops".into())
        );
        // Automated, system, no run_as: unscoped.
        assert_eq!(derive_execution_identity(None, None, true, None), None);
    }

    #[test]
    fn automated_run_block_reasons_in_order() {
        use AutomatedRunBlock::*;
        let reason = automated_run_block_reason;
        assert_eq!(
            reason(Some("alice"), Some("alice"), Some("alice"), true, true),
            None
        );
        assert_eq!(reason(None, Some("root"), None, true, true), None);
        assert_eq!(
            reason(Some("alice"), Some("bob"), Some("alice"), true, true),
            Some(Tainted)
        );
        assert_eq!(
            reason(Some("alice"), Some("bob"), Some("bob"), true, true),
            None
        );
        assert_eq!(
            reason(Some("alice"), Some("alice"), Some("bob"), true, true),
            Some(Tainted)
        );
        assert_eq!(
            reason(Some("alice"), Some("bob"), Some("bob"), false, true),
            Some(Unshared)
        );
        assert_eq!(
            reason(Some("alice"), Some("bob"), Some("bob"), true, false),
            Some(Inactive)
        );
        assert_eq!(
            reason(Some("alice"), Some("bob"), Some("bob"), false, false),
            Some(Unshared)
        );
    }

    /// Why a transfer clears `run_as`. Afterwards the definition runs as its
    /// new owner and blocks as `Tainted`, whose message names the save that
    /// resumes it — never as `Inactive`, whose message names no action anyone
    /// can take, and which is exactly what a surviving `run_as` naming the
    /// departed owner would produce in the rescue case this model exists for.
    #[test]
    fn a_transferred_definition_blocks_as_tainted_and_never_as_inactive() {
        use AutomatedRunBlock::*;
        let reason = automated_run_block_reason;
        // `run_as` cleared: the run identity is the new owner, whose save
        // resumes it.
        assert_eq!(
            reason(Some("bob"), Some("alice"), Some("bob"), true, true),
            Some(Tainted)
        );
        assert!(
            Tainted.to_string().contains("re-saves it"),
            "the block must name an action the new owner can take: {Tainted}"
        );
        // The grantee who maintained it and now owns it wrote what it runs.
        assert_eq!(
            reason(Some("bob"), Some("bob"), Some("bob"), true, true),
            None
        );
        // Had `run_as` survived and named a departed identity, this is what
        // the new owner would have been told instead.
        assert_eq!(
            reason(Some("bob"), Some("carol"), Some("carol"), true, false),
            Some(Inactive)
        );
        assert!(
            !Inactive.to_string().contains("re-save"),
            "the message §6.1 routes away from names no action: {Inactive}"
        );
    }

    /// An administrator who overrides an absent owner's workflow becomes its
    /// last editor, so its automated runs are tainted until somebody
    /// re-approves them. Saving it with `run_as` set to themselves is what they
    /// can do about it — `run_as_write` gives a non-owner exactly that, and
    /// `manages_for_automation` counts their office as manage access, so the
    /// schedule fires as the administrator instead of stopping until an absent
    /// owner returns. Both refusals name an action somebody present can take.
    #[test]
    fn an_administrators_override_leaves_a_way_to_resume_automated_runs() {
        use AutomatedRunBlock::*;
        let reason = automated_run_block_reason;
        // Overridden and still running as its owner: blocked, as any foreign
        // edit is — a member's credentials must not run content they did not
        // approve, whoever wrote it.
        assert_eq!(
            reason(Some("alice"), Some("root"), Some("alice"), true, true),
            Some(Tainted)
        );
        assert!(
            Tainted
                .to_string()
                .contains("whoever edited it saves it with run_as set to themselves"),
            "{Tainted}"
        );
        // Saved with `run_as: me`: the editor is the run identity, and their
        // office is the manage access, so nothing blocks.
        assert_eq!(
            reason(Some("alice"), Some("root"), Some("root"), true, true),
            None
        );
        // Without that standing the same save is `Unshared`, and that message
        // names an administrator rather than the owner alone.
        assert_eq!(
            reason(Some("alice"), Some("root"), Some("root"), false, true),
            Some(Unshared)
        );
        assert!(
            Unshared.to_string().contains("or an administrator"),
            "{Unshared}"
        );
    }

    #[test]
    fn stream_event_json_is_tagged() {
        let started = serde_json::to_value(WorkflowStreamEvent::StepStarted {
            step_id: "s1".into(),
        })
        .unwrap();
        assert_eq!(started["event"], "step_started");
        assert_eq!(started["step_id"], "s1");

        let done = serde_json::to_value(WorkflowStreamEvent::Completed {
            result: serde_json::json!({"ok": true}),
        })
        .unwrap();
        assert_eq!(done["event"], "completed");
        assert_eq!(done["result"]["ok"], true);
    }

    #[test]
    fn validate_required_context_passes_when_all_present() {
        let def = def_with_required_context(&["repo", "ticket_id"]);
        let payload = serde_json::json!({"repo": "x/y", "ticket_id": "DW-1"});
        assert!(validate_required_context(&def, &payload).is_ok());
    }

    #[test]
    fn ticket_prefetch_attempts_prefer_jira() {
        let names: Vec<&str> = TICKET_PREFETCH_ATTEMPTS
            .iter()
            .map(|(s, _, _)| *s)
            .collect();
        assert_eq!(names, vec!["jira", "linear"]);
    }

    #[test]
    fn prefetch_failure_no_adapter_contains_marker_and_ticket() {
        let s = format_prefetch_failure("ORD-1234", false, None);
        assert!(s.contains(PREFETCH_FAILED_MARKER));
        assert!(s.contains("ORD-1234"));
        assert!(s.contains("Jira") && s.contains("Linear"));
    }

    #[test]
    fn prefetch_failure_adapter_error_contains_marker_and_underlying_error() {
        let s = format_prefetch_failure("ORD-9", true, Some("jira: 404 Not Found"));
        assert!(s.contains(PREFETCH_FAILED_MARKER));
        assert!(s.contains("ORD-9"));
        assert!(s.contains("404 Not Found"));
    }

    #[test]
    fn prefetch_failure_attempted_without_error_is_unknown() {
        let s = format_prefetch_failure("ORD-9", true, None);
        assert!(s.contains(PREFETCH_FAILED_MARKER));
        assert!(s.contains("unknown error"));
    }

    #[test]
    fn validate_required_context_errors_with_missing_field_names() {
        let def = def_with_required_context(&["repo", "ticket_id"]);
        let payload = serde_json::json!({"ticket_id": "DW-1"});
        let err = validate_required_context(&def, &payload)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("repo"),
            "error should name the missing field: {err}"
        );
    }

    #[test]
    fn validate_required_context_treats_empty_string_as_missing() {
        let def = def_with_required_context(&["repo"]);
        let payload = serde_json::json!({"repo": "   "});
        assert!(validate_required_context(&def, &payload).is_err());
    }

    #[test]
    fn validate_required_context_error_includes_schema_descriptions() {
        let def = def_with_context(vec![
            ("repo", true, "The repository in owner/repo format"),
            ("ticket_id", true, "The Linear ticket ID"),
        ]);
        let payload = serde_json::json!({"ticket_id": "DW-1"});
        let err = validate_required_context(&def, &payload)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("The repository in owner/repo format"),
            "error should embed the field description: {err}"
        );
        assert!(
            err.contains("repo (required)"),
            "error should label requiredness: {err}"
        );
    }

    #[test]
    fn validate_required_context_lists_provided_fields() {
        let def = def_with_required_context(&["repo", "ticket_id"]);
        let payload = serde_json::json!({"ticket_id": "DW-1"});
        let err = validate_required_context(&def, &payload)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("You provided"),
            "error should echo provided fields so the agent can spot typos: {err}"
        );
        assert!(err.contains("ticket_id"));
    }

    #[test]
    fn validate_required_context_rejects_unknown_field_with_did_you_mean() {
        let def = def_with_required_context(&["repo", "ticket_id"]);
        let payload = serde_json::json!({"repo": "x/y", "ticket_id": "DW-1", "ticked": "stale"});
        let err = validate_required_context(&def, &payload)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("ticked"),
            "error should name the unknown field: {err}"
        );
        assert!(
            err.contains("did you mean 'ticket_id'"),
            "should suggest the closest match: {err}"
        );
    }

    #[test]
    fn validate_required_context_rejects_workspace_when_repo_expected() {
        // The exact failure mode we saw in prod: agent sent `workspace` instead
        // of `repo`. Levenshtein distance between them is 7, so no suggestion;
        // we still reject the unknown field so the agent sees its mistake.
        let def = def_with_required_context(&["repo", "ticket_id"]);
        let payload = serde_json::json!({"ticket_id": "DW-1", "workspace": "fabrikamcrm"});
        let err = validate_required_context(&def, &payload)
            .unwrap_err()
            .to_string();
        assert!(err.contains("workspace"), "must flag unknown field: {err}");
        assert!(
            err.contains("repo"),
            "schema must list the real field name: {err}"
        );
    }

    #[test]
    fn validate_required_context_allows_runtime_managed_keys() {
        let def = def_with_required_context(&["repo", "ticket_id"]);
        let payload = serde_json::json!({
            "repo": "x/y",
            "ticket_id": "DW-1",
            "_channel_id": "C123",
            "_thread_id": "1.2",
            "_worktree_path": "/tmp/wt",
            "_cancellation_reason": "user",
        });
        assert!(validate_required_context(&def, &payload).is_ok());
    }

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("ticked", "ticket_id"), 3);
        assert_eq!(levenshtein("ticket", "ticket_id"), 3);
        assert_eq!(levenshtein("repo", "repo"), 0);
        // Verify the prod-observed pair is far apart enough not to suggest.
        assert!(levenshtein("workspace", "repo") > 3);
    }

    fn instance_with_vars(vars: serde_json::Value) -> WorkflowInstance {
        let trigger = TriggerInfo {
            source: TriggerSource::Cli,
            payload: serde_json::json!({}),
        };
        let mut instance = WorkflowInstance::new("test", trigger, "step1", None);
        // Assigned post-construction, as the engine writes its own `_…` keys:
        // the constructor strips them from trigger payloads by design.
        instance.variables = vars;
        instance
    }

    #[test]
    fn recover_failed_to_paused_drops_status_and_failure_marker() {
        let mut instance = instance_with_vars(serde_json::json!({}));
        WorkflowExecutor::fail(&mut instance, "Cancelled by user");
        assert_eq!(instance.status, InstanceStatus::Failed);
        assert_eq!(
            instance.variables["_failure_reason"],
            serde_json::Value::String("Cancelled by user".to_string())
        );

        recover_failed_to_paused(&mut instance);

        assert_eq!(instance.status, InstanceStatus::Paused);
        assert!(
            instance.variables.get("_failure_reason").is_none(),
            "_failure_reason must be cleared so the resumed step doesn't see stale context"
        );
    }

    #[test]
    fn park_for_usage_cap_holds_the_run_without_failing_it() {
        let mut instance = instance_with_vars(serde_json::json!({}));
        instance.current_step_id = Some("investigate".to_string());

        park_for_usage_cap(&mut instance, "You've reached your Opus limit");

        assert_eq!(
            instance.status,
            InstanceStatus::Paused,
            "a spent usage window holds the run; it has not failed"
        );
        assert_eq!(
            instance.current_step_id.as_deref(),
            Some("investigate"),
            "the held step stays current, so resuming re-runs it"
        );
        assert!(
            instance.variables.get("_failure_reason").is_none(),
            "a held run must carry no failure marker"
        );
        assert!(held_for_usage_cap(&instance.variables));
    }

    #[test]
    fn a_suspension_holds_the_run_on_its_step_and_only_its_own_hold_is_lifted() {
        let mut instance = instance_with_vars(serde_json::json!({"_run_as": "ada"}));
        instance.current_step_id = Some("investigate".to_string());
        WorkflowExecutor::fail(&mut instance, "refused");

        park_for_suspension(&mut instance);
        assert_eq!(instance.status, InstanceStatus::Paused);
        assert_eq!(instance.current_step_id.as_deref(), Some("investigate"));
        assert!(instance.variables.get("_failure_reason").is_none());
        assert!(held_for_suspension(&instance.variables));
        assert!(
            !held_for_usage_cap(&instance.variables),
            "the usage-cap sweep leaves it alone"
        );

        take_suspension_hold(&mut instance);
        assert!(!held_for_suspension(&instance.variables));
        let mut paused_by_hand = instance_with_vars(serde_json::json!({"_run_as": "ada"}));
        paused_by_hand.status = InstanceStatus::Paused;
        assert!(
            !held_for_suspension(&paused_by_hand.variables),
            "a run paused by a person carries no hold to lift"
        );
    }

    #[test]
    fn park_for_usage_cap_clears_an_earlier_failure_marker() {
        let mut instance = instance_with_vars(serde_json::json!({}));
        WorkflowExecutor::fail(&mut instance, "something earlier");

        park_for_usage_cap(&mut instance, "You've reached your Fable limit");

        assert!(
            instance.variables.get("_failure_reason").is_none(),
            "holding a run must not leave it looking failed"
        );
    }

    #[test]
    fn clearing_the_hold_stops_the_sweep_seeing_it() {
        let mut instance = instance_with_vars(serde_json::json!({"_channel_id": "C123"}));
        park_for_usage_cap(&mut instance, "You've reached your Opus limit");
        assert!(held_for_usage_cap(&instance.variables));

        clear_usage_cap_hold(&mut instance);

        assert!(
            !held_for_usage_cap(&instance.variables),
            "a resumed run must not be picked up again by the sweep"
        );
        assert!(
            instance.variables.get("_usage_cap_reason").is_none(),
            "both markers go, or a later unrelated failure looks like a cap"
        );
        assert_eq!(
            instance.variables["_channel_id"],
            serde_json::Value::String("C123".to_string()),
            "clearing the hold must not disturb the run's other variables"
        );
    }

    #[test]
    fn a_run_that_was_never_held_is_not_swept() {
        let instance = instance_with_vars(serde_json::json!({"_channel_id": "C123"}));
        assert!(!held_for_usage_cap(&instance.variables));
    }

    /// The distinction the whole feature rests on: a spent subscription window
    /// is worth waiting hours for, a 429 clears in minutes and the engine
    /// chain already deals with it. Only the first parks a run.
    #[test]
    fn only_a_spent_window_holds_a_run_not_a_transient_rate_limit() {
        assert!(is_usage_capped("You've reached your Opus limit"));
        assert!(!is_usage_capped(
            "API Error: Request rejected (429) · This request would exceed your organization's rate limit"
        ));
    }

    /// The halt reason for a failed coding round is not the CLI's text: it is
    /// a prefixed summary with the narration quoted line by line. The hold has
    /// to be decided on *that* string, so every refusal the detector knows has
    /// to survive the quoting — not just the one phrasing whose fallback is a
    /// loose substring search.
    #[test]
    fn every_usage_refusal_survives_the_quoting_a_halt_reason_puts_it_behind() {
        let halt_reason = |cli_text: &str| {
            format!(
                "claude-code failed (exit 1). Halted to prevent API cost fallback.\n{}",
                narration_tail(cli_text)
            )
        };

        for notice in [
            "You've reached your Opus limit",
            "You've hit your usage limit",
            "You're out of usage credits",
            "You're out of extra usage",
            "Your org is out of usage credits",
            "Your seat type doesn't include access to this model",
            "Your usage allocation has been disabled",
        ] {
            let reason = halt_reason(&format!(
                "Working on the ticket…\nRead src/main.rs\n{notice}"
            ));
            assert!(
                is_usage_capped(&reason),
                "a run must be held for {notice:?}, but the quoted halt reason was not \
                 recognised:\n{reason}"
            );
        }
    }

    /// A long run's narration is cut to its last 600 characters and the
    /// surviving text opens with an ellipsis. A refusal that lands on that
    /// boundary must still be recognised, or the longest runs — the ones most
    /// likely to exhaust a window — are exactly the ones that fail to park.
    #[test]
    fn a_refusal_on_the_truncation_boundary_still_holds_the_run() {
        let notice = "You're out of usage credits";
        // Pad so the notice sits at the 600-character cut, where the tail
        // opens mid-line and picks up the ellipsis.
        for pad in [560, 580, 590, 600, 620] {
            let cli_text = format!("{}\n{notice}", "narration line\n".repeat(pad / 15));
            let reason = format!(
                "claude-code failed (exit 1). Halted to prevent API cost fallback.\n{}",
                narration_tail(&cli_text)
            );
            assert!(
                is_usage_capped(&reason),
                "a run must be held when the refusal survives truncation at pad {pad}:\n{reason}"
            );
        }
    }

    /// The other half: ordinary failures, and transient limits, must still
    /// fail the run after the same quoting. A workflow that parks on every
    /// error would never report anything.
    #[test]
    fn a_quoted_ordinary_failure_still_fails_the_run() {
        let reason = format!(
            "claude-code failed (exit 1). Halted to prevent API cost fallback.\n{}",
            narration_tail(
                "Working on the ticket…\nAPI Error: Request rejected (429) · This request \
                 would exceed your organization's rate limit of 30,000 input tokens per minute"
            )
        );
        assert!(
            !is_usage_capped(&reason),
            "a 429 clears in minutes; the engine chain handles it and the run must not park"
        );

        let reason = format!(
            "claude-code failed (exit 1). Halted to prevent API cost fallback.\n{}",
            narration_tail("Working on the ticket…\ncargo build failed: unresolved import")
        );
        assert!(!is_usage_capped(&reason));
    }

    #[test]
    fn recover_failed_to_paused_preserves_other_variables() {
        let mut instance = instance_with_vars(
            serde_json::json!({"_channel_id": "C123", "_human_feedback": "looks good"}),
        );
        WorkflowExecutor::fail(&mut instance, "boom");
        recover_failed_to_paused(&mut instance);
        assert_eq!(instance.variables["_channel_id"], "C123");
        assert_eq!(instance.variables["_human_feedback"], "looks good");
    }

    #[test]
    fn render_terminal_status_failure_includes_step_and_error() {
        let inst = instance_with_vars(serde_json::json!({}));
        let msg = render_terminal_status(
            "abc-123",
            Some(&inst),
            &TerminalOutcome::Failed {
                error: "cannot resolve template variable",
            },
        );
        assert!(msg.contains("Workflow failed"));
        assert!(msg.contains("`test`"));
        assert!(msg.contains("`step1`"));
        assert!(msg.contains("`abc-123`"));
        assert!(msg.contains("cannot resolve template variable"));
    }

    #[test]
    fn render_terminal_status_cancelled_includes_reason() {
        let inst = instance_with_vars(serde_json::json!({}));
        let msg = render_terminal_status(
            "abc-123",
            Some(&inst),
            &TerminalOutcome::Cancelled {
                reason: "Cancelled by user",
            },
        );
        assert!(msg.contains("Workflow cancelled"));
        assert!(msg.contains("Cancelled by user"));
    }

    fn pilot_result(
        text: &str,
        exit_code: i32,
        phases_completed: u32,
        phases_total: u32,
        turns: u32,
    ) -> crate::claude_pilot::PilotResult {
        crate::claude_pilot::PilotResult {
            text: text.to_string(),
            exit_code,
            duration: std::time::Duration::from_secs(1),
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.1234,
            num_turns: turns,
            session_id: None,
            phases_completed,
            phases_total,
        }
    }

    fn step_with_phases(
        phases: &[(&str, u32)],
    ) -> workforce_orchestrator::workflow::definition::StepDefinition {
        let toml_str = format!(
            r#"
[workflow]
id = "t"
name = "t"

[[step]]
id = "review"
name = "Code Review"
task = "orchestrated by phases"
engine = "claude-pilot"
{phases}
"#,
            phases = phases
                .iter()
                .map(|(name, turns)| format!(
                    "[[step.phases]]\nname = \"{name}\"\ntask = \"\"\nmax_turns = {turns}\n"
                ))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let def =
            workforce_orchestrator::workflow::definition::WorkflowDefinition::from_toml(&toml_str)
                .expect("test def parses");
        def.steps.into_iter().next().unwrap()
    }

    #[test]
    fn format_pilot_failure_names_the_failed_phase() {
        let def = step_with_phases(&[("diff", 5), ("review", 5), ("gates", 8), ("verdict", 2)]);
        let result = pilot_result("lint passed\ntypecheck running...", 1, 2, 4, 11);
        let msg = format_pilot_failure_message("review", Some(&def), &result, "abc-123");
        assert!(
            msg.contains("phase `gates`"),
            "must name the failed phase: {msg}"
        );
        assert!(msg.contains("2/4"), "must show phase progress: {msg}");
        assert!(
            msg.contains("exceeded the phase turn budget (8)"),
            "must explain likely cause: {msg}"
        );
        assert!(
            msg.contains("typecheck running"),
            "must include output tail: {msg}"
        );
    }

    #[test]
    fn format_pilot_failure_under_budget_infers_tool_failure() {
        let def = step_with_phases(&[("setup", 8), ("code", 25)]);
        let result = pilot_result("cargo clippy: error[E0432]", 1, 1, 2, 6);
        let msg = format_pilot_failure_message("implement", Some(&def), &result, "abc-123");
        assert!(msg.contains("phase `code`"));
        assert!(
            msg.contains("tool call"),
            "under-budget failure should blame a tool: {msg}"
        );
    }

    #[test]
    fn format_pilot_failure_handles_missing_step_def() {
        let result = pilot_result("", 1, 0, 4, 5);
        let msg = format_pilot_failure_message("review", None, &result, "abc-123");
        assert!(msg.contains("phase `unknown`"));
    }

    #[test]
    fn tail_chars_short_string_unchanged() {
        assert_eq!(tail_chars("hello", 10), "hello");
    }

    #[test]
    fn tail_chars_long_string_truncated_to_end() {
        let s = "a".repeat(100) + "TAIL";
        assert_eq!(tail_chars(&s, 4), "TAIL");
    }

    #[test]
    fn render_terminal_status_handles_missing_instance() {
        let msg =
            render_terminal_status("abc-123", None, &TerminalOutcome::Failed { error: "boom" });
        assert!(msg.contains("`?`"));
        assert!(msg.contains("`unknown`"));
        assert!(msg.contains("boom"));
    }

    #[test]
    fn step_allowed_tools_defaults_to_the_file_and_shell_set() {
        let mut exec = exec_with_phases(Vec::new());
        assert_eq!(
            step_allowed_tools(&exec),
            DEFAULT_STEP_TOOLS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        exec.allowed_tools = vec!["Read".into(), "WebFetch".into()];
        assert_eq!(step_allowed_tools(&exec), vec!["Read", "WebFetch"]);
    }

    fn served(engine: &str, cost_usd: f64) -> coding_engine::CliRoundResult {
        coding_engine::CliRoundResult {
            engine: engine.into(),
            model: Some("claude-sonnet-5".into()),
            input_tokens: 2_000,
            output_tokens: 300,
            cost_usd,
            num_turns: 4,
            ..Default::default()
        }
    }

    fn handed(engine: &str, cost_usd: f64) -> EngineAttempt {
        EngineAttempt {
            engine: engine.into(),
            model: Some("claude-opus-4-8".into()),
            input_tokens: 1_000,
            output_tokens: 100,
            cost_usd,
            error: None,
        }
    }

    fn step_base() -> StepProvenance {
        StepProvenance {
            definition_hash: Some("ab".repeat(32)),
            ..Default::default()
        }
    }

    #[test]
    fn a_round_that_nothing_handed_over_to_books_as_before() {
        let (booked, cost) = round_booking(step_base(), "claude-code", &Ok(served("", 0.4)));
        assert_eq!(
            booked,
            StepProvenance {
                engine: Some("claude-code".into()),
                model: Some("claude-sonnet-5".into()),
                input_tokens: Some(2_000),
                output_tokens: Some(300),
                ..step_base()
            }
        );
        assert_eq!(cost, 0.4);

        let (named, _) = round_booking(
            step_base(),
            "claude-code",
            &Ok(served("claude-code:bedrock", 0.4)),
        );
        assert_eq!(named.engine.as_deref(), Some("claude-code:bedrock"));
    }

    #[test]
    fn handed_over_attempts_fold_into_the_steps_tokens_and_cost() {
        let result = coding_engine::CliRoundResult {
            handed_over: vec![handed("claude-code", 0.5), handed("codex", 0.25)],
            ..served("claude-code:bedrock", 1.0)
        };
        let (booked, cost) = round_booking(step_base(), "claude-code", &Ok(result));
        assert_eq!(booked.engine.as_deref(), Some("claude-code:bedrock"));
        assert_eq!(booked.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(booked.input_tokens, Some(4_000));
        assert_eq!(booked.output_tokens, Some(500));
        assert_eq!(
            booked.handed_over,
            vec![handed("claude-code", 0.5), handed("codex", 0.25)]
        );
        assert_eq!(booked.definition_hash, step_base().definition_hash);
        assert!((cost - 1.75).abs() < 1e-9);
    }

    #[test]
    fn an_errored_chain_books_its_handed_over_attempts_under_the_engine_asked_for() {
        let errored = coding_engine::with_handed_over(
            Err(eyre!("codex error: stream closed")),
            vec![handed("claude-code", 0.5)],
        );
        let (booked, cost) = round_booking(step_base(), "claude-code", &errored);
        assert_eq!(
            booked,
            StepProvenance {
                engine: Some("claude-code".into()),
                model: None,
                input_tokens: Some(1_000),
                output_tokens: Some(100),
                handed_over: vec![handed("claude-code", 0.5)],
                ..step_base()
            }
        );
        assert_eq!(cost, 0.5);
    }

    #[test]
    fn an_errored_round_with_no_attempts_books_nothing() {
        let (booked, cost) = round_booking(step_base(), "codex", &Err(eyre!("spawn failed")));
        assert_eq!(
            booked,
            StepProvenance {
                engine: Some("codex".into()),
                input_tokens: Some(0),
                output_tokens: Some(0),
                ..step_base()
            }
        );
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn the_target_repo_comes_from_a_prior_steps_output() {
        let none = instance_with_vars(serde_json::json!({}));
        assert_eq!(WorkflowRuntime::target_repo(&none), None);

        let picked = instance_with_vars(serde_json::json!({
            "steps": { "pick": { "ticket_id": "ORD-2613", "target_repo": "o/r" } }
        }));
        assert_eq!(WorkflowRuntime::target_repo(&picked), Some("o/r"));
    }

    #[test]
    fn a_closed_runner_is_the_only_error_that_reads_as_a_restart() {
        assert!(runner_lost(
            "Claude Code step could not get a workspace on the coding worker: \
             secure command runner unavailable: connection closed before completion"
        ));
        assert!(!runner_lost("Claude Code failed (exit 1)."));
    }

    #[test]
    fn the_workspace_repos_come_from_a_prior_steps_output() {
        let none = instance_with_vars(serde_json::json!({}));
        assert_eq!(WorkflowRuntime::workspace_repos(&none), None);

        let picked = instance_with_vars(serde_json::json!({
            "steps": { "pick": { "target_repo": "o/r", "workspace_repos": "o/r o/s" } }
        }));
        assert_eq!(WorkflowRuntime::workspace_repos(&picked), Some("o/r o/s"));
    }

    fn slugs(project: &workforce_projects::Project) -> Vec<String> {
        project
            .repos
            .iter()
            .map(|repo| {
                let workforce_projects::RepoSource::Github { owner, repo: name } = &repo.source;
                assert_eq!(&repo.alias, name, "a repo is checked out under its name");
                format!("{owner}/{name}")
            })
            .collect()
    }

    #[test]
    fn step_project_names_the_target_repo_or_none() {
        let empty = step_project("a", None, None).unwrap();
        assert_eq!(empty.name, "workflow");
        assert!(empty.repos.is_empty());
        assert!(
            step_project("a", None, Some("acme/app"))
                .unwrap()
                .repos
                .is_empty(),
            "workspace repos without a target name no checkout"
        );

        let rails = step_project("a", Some("acme/WebApi"), None).unwrap();
        assert_eq!(rails.name, "workflow:acme/WebApi");
        assert_eq!(slugs(&rails), ["acme/WebApi"]);
        for workspace in ["", " ,\n", "acme/WebApi"] {
            let same = step_project("a", Some("acme/WebApi"), Some(workspace)).unwrap();
            assert_eq!(same.repos, rails.repos, "{workspace:?}");
        }

        for bad in NOT_OWNER_REPO {
            assert!(step_project("a", Some(bad), None).is_err(), "{bad}");
        }
    }

    const NOT_OWNER_REPO: [&str; 9] = [
        "rails",
        "/rails",
        "owner/",
        "a/b/c",
        "owner/..",
        "./rails",
        "owner/rails;id",
        "owner/$(id)",
        "owner/rails`id`",
    ];

    #[test]
    fn step_project_puts_the_target_first_then_each_workspace_repo_in_order() {
        let project = step_project(
            "a",
            Some("acme/WebApi"),
            Some(
                "acme/app,acme/worker\n \
                 acme/WebApps",
            ),
        )
        .unwrap();
        assert_eq!(
            project.name, "workflow:acme/WebApi",
            "the scope stays keyed by the target"
        );
        assert_eq!(
            slugs(&project),
            ["acme/WebApi", "acme/app", "acme/worker", "acme/WebApps",]
        );
    }

    #[test]
    fn step_project_dedupes_workspace_repos_case_insensitively_keeping_the_target() {
        let project = step_project(
            "a",
            Some("acme/app"),
            Some(
                "Acme/APP acme/WebApi, \
                 acme/webapi acme/app",
            ),
        )
        .unwrap();
        assert_eq!(slugs(&project), ["acme/app", "acme/WebApi"]);
    }

    #[test]
    fn step_project_refuses_a_workspace_entry_that_is_not_owner_slash_repo() {
        for bad in NOT_OWNER_REPO {
            let workspace = format!("acme/WebApi {bad}");
            let err = step_project("a", Some("acme/app"), Some(&workspace)).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("workspace_repos entry {bad:?} is not owner/repo")
            );
        }
    }

    #[test]
    fn step_project_refuses_repos_of_two_owners_that_would_share_a_checkout() {
        for clash in ["fork/app", "fork/APP"] {
            let err = step_project("a", Some("acme/app"), Some(clash)).unwrap_err();
            assert!(
                err.to_string()
                    .contains(&format!("names acme/app and {clash}")),
                "{err}"
            );
        }
        assert!(
            step_project("a", Some("acme/app"), Some("acme/rails fork/rails")).is_err(),
            "between workspace entries too"
        );
    }

    #[test]
    fn resolve_step_workdir_prefers_worktree_path() {
        let instance = instance_with_vars(serde_json::json!({
            "_worktree_path": "/some/worktree",
        }));
        let workdir = WorkflowRuntime::resolve_step_workdir(&instance, std::path::Path::new("/ws"));
        assert_eq!(workdir, std::path::PathBuf::from("/some/worktree"));
    }

    #[test]
    fn resolve_step_workdir_falls_back_to_target_repo_when_worktree_missing() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let repo_dir = workspace.join("repos").join("driftwerk/workforce");
        std::fs::create_dir_all(&repo_dir).unwrap();

        let instance = instance_with_vars(serde_json::json!({
            "steps": {
                "understand": { "target_repo": "driftwerk/workforce" }
            }
        }));
        let workdir = WorkflowRuntime::resolve_step_workdir(&instance, workspace);
        assert_eq!(workdir, repo_dir);
    }

    #[test]
    fn resolve_step_workdir_skips_missing_target_repo_dir() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        // No `repos/driftwerk/workforce/` created — fallback should kick in.
        let instance = instance_with_vars(serde_json::json!({
            "steps": {
                "understand": { "target_repo": "driftwerk/workforce" }
            }
        }));
        let workdir = WorkflowRuntime::resolve_step_workdir(&instance, workspace);
        assert_eq!(workdir, workspace.join("repos"));
    }

    #[test]
    fn resolve_step_workdir_default_fallback() {
        let instance = instance_with_vars(serde_json::json!({}));
        let workdir = WorkflowRuntime::resolve_step_workdir(&instance, std::path::Path::new("/ws"));
        assert_eq!(workdir, std::path::PathBuf::from("/ws/repos"));
    }

    #[test]
    fn strip_tool_json_removes_single_line_json() {
        let input = r#"Code Review

{"exit_code":0,"stderr":"","stdout":"authSchema.ts\ncustomerSchema.ts\n"}

All schemas look good."#;
        let result = strip_tool_json(input);
        assert_eq!(result, "Code Review\n\nAll schemas look good.");
    }

    #[test]
    fn strip_tool_json_removes_multiline_json() {
        let input = r#"Review complete.

{
  "author": "bot",
  "state": "open",
  "number": 27
}

Everything looks fine."#;
        let result = strip_tool_json(input);
        assert_eq!(result, "Review complete.\n\nEverything looks fine.");
    }

    #[test]
    fn strip_tool_json_preserves_normal_text() {
        let input = "This is a normal review.\nNo JSON here.\nAll good.";
        let result = strip_tool_json(input);
        assert_eq!(result, input);
    }

    #[test]
    fn strip_tool_json_preserves_template_braces() {
        let input = "Use {% if condition %} for templates.";
        let result = strip_tool_json(input);
        assert_eq!(result, input);
    }

    #[test]
    fn strip_tool_json_empty_after_stripping() {
        let input = r#"{"exit_code":0,"stdout":"ok"}"#;
        let result = strip_tool_json(input);
        assert_eq!(result, "");
    }

    fn exec_with_phases(
        phases: Vec<workforce_orchestrator::workflow::definition::PhaseDefinition>,
    ) -> workforce_orchestrator::workflow::engine::StepExecution {
        workforce_orchestrator::workflow::engine::StepExecution {
            step_id: "s".into(),
            agent_id: Some("a".into()),
            task_prompt: "fallback prompt".into(),
            skill: None,
            mode: workforce_orchestrator::workflow::engine::StepMode::Agent,
            max_rounds: Some(7),
            model: None,
            context_eviction_interval: None,
            context_eviction_keep_last: 3,
            report: None,
            skills: Vec::new(),
            preferred_tools: Vec::new(),
            exclusive_tools: false,
            engine: "default".into(),
            effort: None,
            timeout_secs: None,
            allowed_tools: Vec::new(),
            fallback: Vec::new(),
            phases,
            persona: None,
            task: "single-shot task".into(),
            correction: String::new(),
            decide_first: None,
        }
    }

    fn phase(
        name: &str,
        task: &str,
        max_turns: u32,
        tools: &[&str],
    ) -> workforce_orchestrator::workflow::definition::PhaseDefinition {
        workforce_orchestrator::workflow::definition::PhaseDefinition {
            name: name.into(),
            task: task.into(),
            max_turns,
            allowed_tools: tools.iter().map(|t| (*t).into()).collect(),
            mcp: false,
        }
    }

    #[test]
    fn a_phased_step_that_was_sent_back_is_told_why_in_its_first_phase() {
        let exec = StepExecution {
            correction: "\n\n## Correction required\n\n'count' is missing".into(),
            ..exec_with_phases(vec![
                phase("plan", "Plan it.", 3, &[]),
                phase("write", "Write it.", 5, &[]),
            ])
        };
        let calls = build_phase_calls(&exec, None);
        assert_eq!(
            calls[0].prompt,
            "Plan it.\n\n## Correction required\n\n'count' is missing"
        );
        assert_eq!(calls[1].prompt, "Write it.");
    }

    #[test]
    fn build_phase_calls_returns_single_shot_when_no_phases() {
        let exec = exec_with_phases(Vec::new());
        let base = Some(vec!["github.pr_get".into()]);
        let calls = build_phase_calls(&exec, base.clone());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "");
        assert_eq!(calls[0].prompt, "fallback prompt");
        assert_eq!(calls[0].max_rounds, Some(7));
        assert_eq!(calls[0].whitelist, base);
    }

    #[test]
    fn build_phase_calls_returns_one_call_per_phase() {
        let exec = exec_with_phases(vec![
            phase("verify", "run gh pr view", 4, &["Bash"]),
            phase("close", "update linear", 5, &["linear.issue_update"]),
        ]);
        let calls = build_phase_calls(&exec, None);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "verify");
        assert_eq!(calls[0].prompt, "run gh pr view");
        assert_eq!(calls[0].max_rounds, Some(4));
        assert_eq!(
            calls[0].whitelist.as_deref(),
            Some(&["Bash".to_string()][..])
        );
        assert_eq!(calls[1].name, "close");
        assert_eq!(calls[1].max_rounds, Some(5));
        assert_eq!(
            calls[1].whitelist.as_deref(),
            Some(&["linear.issue_update".to_string()][..])
        );
    }

    #[test]
    fn build_phase_calls_phase_with_empty_allowed_tools_restricts_to_none() {
        let exec = exec_with_phases(vec![phase("think", "no tools", 3, &[])]);
        let calls = build_phase_calls(&exec, Some(vec!["ignored".into()]));
        assert_eq!(calls.len(), 1);
        // Phased steps override the step-level whitelist; an empty allowed_tools
        // means "only system tools" once the filter in serve.rs:4755 applies.
        assert_eq!(calls[0].whitelist.as_deref(), Some(&[] as &[String]));
    }

    #[test]
    fn build_phase_calls_phased_overrides_step_level_whitelist() {
        let exec = exec_with_phases(vec![phase("only-bash", "run it", 2, &["Bash"])]);
        let base = Some(vec!["github.pr_get".into(), "linear.issue_update".into()]);
        let calls = build_phase_calls(&exec, base);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].whitelist.as_deref(),
            Some(&["Bash".to_string()][..])
        );
    }

    #[test]
    fn build_phase_calls_phased_uses_phase_max_turns_not_step_max_rounds() {
        let exec = exec_with_phases(vec![phase("p", "t", 11, &[])]);
        let calls = build_phase_calls(&exec, None);
        assert_eq!(calls[0].max_rounds, Some(11));
    }

    /// A workflow `id` that runs as `example`, whose one step names an unknown
    /// tool so a run fails fast and offline.
    fn agent_workflow(id: &str) -> WorkflowDefinition {
        WorkflowDefinition::from_toml(&format!(
            "[workflow]\nid = \"{id}\"\nname = \"Example\"\nagent = \"example\"\nmax_concurrent = 3\n\n\
             [[step]]\nid = \"s1\"\nname = \"s1\"\ntask = \"{{}}\"\ntool = \"nope.nope\"\nexit = true\n"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn every_start_path_records_the_agent_the_workflow_declares() {
        let runtime = test_runtime().await;
        runtime
            .definitions
            .write()
            .await
            .insert("wf-agent".into(), owned(agent_workflow("wf-agent"), None));

        let started = runtime
            .start_workflow("wf-agent", serde_json::json!({}), None, None, None, None)
            .await
            .expect("start");
        let triggered = runtime
            .start_workflow_from_trigger("wf-agent", webhook_trigger(), None, None, None, None)
            .await
            .expect("trigger start");
        let (streamed, _events) = runtime
            .start_workflow_streaming("wf-agent", webhook_trigger(), None)
            .await
            .expect("streaming start");

        for id in [started, triggered, streamed] {
            let instance = runtime.load_instance(&id).expect("instance saved");
            assert_eq!(instance.agent_id.as_deref(), Some("example"), "{id}");
        }
    }

    #[tokio::test]
    async fn declared_agents_name_only_loaded_definitions_that_declare_one() {
        let runtime = test_runtime().await;
        let declaring = owned(agent_workflow("orders"), Some("owner-1"));
        let key = declaring.key();
        {
            let mut definitions = runtime.definitions.write().await;
            definitions.insert(key.clone(), declaring);
            definitions.insert("smoke".into(), owned(queueable_def(1), None));
        }

        let declared = runtime
            .declared_agents([key.as_str(), "smoke", "missing"])
            .await;

        assert_eq!(declared, HashMap::from([(key, "example".to_string())]));
    }

    #[tokio::test]
    async fn a_terminal_status_posts_through_the_surface_of_the_agent_the_run_records() {
        let runtime = test_runtime().await;
        let example_surface = Arc::new(RecordingSurface::default());
        let first_slack = Arc::new(RecordingSurface::default());
        runtime.set_agent_surfaces(
            &HashMap::from([("W1".to_string(), "example".to_string())]),
            &HashMap::from([(
                "W1".to_string(),
                example_surface.clone() as Arc<dyn ChannelSurface>,
            )]),
        );
        let mut registry = SurfaceRegistry::new();
        registry.register(SurfaceKey::workspace("slack", "W2"), first_slack.clone());
        runtime.set_surface_registry(Arc::new(tokio::sync::RwLock::new(registry)));
        let instance =
            WorkflowExecutor::start(&agent_workflow("wf-agent"), webhook_trigger(), None).unwrap();
        runtime.save_instance(&instance).unwrap();

        runtime
            .post_terminal_status(
                &instance.id,
                Some("C1"),
                None,
                TerminalOutcome::Failed { error: "boom" },
            )
            .await;

        assert_eq!(example_surface.sent.lock().await.len(), 1);
        assert!(first_slack.sent.lock().await.is_empty());
    }

    /// The orders report Slack refused as one post: a long status goes out
    /// as an ordered run of messages, the later ones threaded under the first
    /// when the call had no thread, and the first's id is what comes back.
    #[tokio::test]
    async fn a_long_status_is_delivered_as_a_threaded_run_of_messages() {
        let surface = Arc::new(RecordingSurface::default());
        let runtime = test_runtime().await;
        runtime.set_agent_surfaces(
            &HashMap::from([("W1".to_string(), "agent-a".to_string())]),
            &HashMap::from([("W1".to_string(), surface.clone() as Arc<dyn ChannelSurface>)]),
        );
        let report = (0..100)
            .map(|i| {
                format!(
                    "- [ORD-{i}](https://example.test/browse/ORD-{i}) {}",
                    "note ".repeat(25).trim_end()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let first = runtime
            .send_status(&report, Some("C1"), None, Some("agent-a"), None)
            .await;

        let sent = surface.sent.lock().await;
        assert_eq!(first.as_deref(), Some("m1"));
        assert!(sent.len() > 1, "expected a run of messages: {}", sent.len());
        assert_eq!(sent[0].thread_id, None);
        assert!(sent[1..]
            .iter()
            .all(|m| m.thread_id.as_deref() == Some("m1")));
        assert_eq!(
            sent.iter().map(|m| m.text.as_str()).collect::<String>(),
            report
        );
    }

    /// The live step-status line is rendered on three paths (event, heartbeat,
    /// flush) from a single redacted `base_text`. Lock that invariant: a secret
    /// in a tool's detail must never reach Slack on any of them.
    #[tokio::test]
    async fn consume_step_progress_redacts_secrets_on_every_emit_path() {
        let surface = Arc::new(RecordingSurface::default());
        let (tx, rx) = tokio::sync::mpsc::channel(8);

        let secret = format!("ghp_{}", "a".repeat(40));
        tx.send(ProgressEvent::ToolStart {
            name: "bash".into(),
            round: 0,
            detail: format!("git push https://{secret}@github.com/x"),
        })
        .await
        .unwrap();
        drop(tx); // end the stream so the throttled line goes out via the flush path

        consume_step_progress(
            rx,
            Some(surface.clone() as Arc<dyn ChannelSurface>),
            Some("C1".into()),
            Some("T1".into()),
            None, // no step message → no emoji reactions to mock
        )
        .await;

        let sent: Vec<String> = surface
            .sent
            .lock()
            .await
            .iter()
            .map(|m| m.text.clone())
            .collect();
        assert!(!sent.is_empty(), "expected a status line to be emitted");
        assert!(
            sent.iter().all(|t| !t.contains(&secret)),
            "raw secret leaked into a status line: {sent:?}"
        );
        assert!(
            sent.iter().any(|t| t.contains("[REDACTED GITHUB_PAT]")),
            "expected the secret to be redacted: {sent:?}"
        );
    }
}

/// A hub whose people and workflow runs the steering tests on every surface
/// share ([`crate::run_steering`]).
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::run_steering::Steerer;
    use workforce_common::channels::{ChannelCapabilities, SentMessage};
    use workforce_identity::types::{Identity, IdentityRole};
    use workforce_orchestrator::workflow::instance::Actor;

    pub(crate) async fn runtime() -> Arc<WorkflowRuntime> {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = workforce_common::settings::WorkforceConfig::default();
        cfg.hub.data_dir = tmp.path().to_string_lossy().to_string();
        cfg.memory.db_path = std::path::Path::new(&cfg.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        cfg.llm.api_key = Some("test-key".into());
        let workflows_dir = tmp.path().join("workflows");
        std::mem::forget(tmp);
        let service = crate::serve::AgentService::from_config(cfg, None)
            .await
            .expect("test service");
        let telemetry = Arc::clone(&service.telemetry_store);
        Arc::new(WorkflowRuntime::new(service, telemetry, workflows_dir))
    }

    /// Alice owns the workflow and starts runs of it, Bob is another member,
    /// Carol an administrator.
    pub(crate) struct Hub {
        pub runtime: Arc<WorkflowRuntime>,
        pub service: Arc<AgentService>,
        pub alice: String,
        pub bob: String,
        pub carol: String,
    }

    impl Hub {
        pub(crate) fn host(&self) -> Arc<dyn crate::hosts::WorkflowHost> {
            Arc::clone(&self.runtime) as Arc<dyn crate::hosts::WorkflowHost>
        }
    }

    /// A surface that keeps every message posted to it, edited on it and
    /// deleted from it. It opens a direct message with `U…` as `D-U…`, knows
    /// the Slack users in `slack_users` by email, and refuses a send to a
    /// channel in `failing` with that channel's error.
    #[derive(Default)]
    pub(crate) struct RecordingSurface {
        pub sent: tokio::sync::Mutex<Vec<OutboundMessage>>,
        pub updated: tokio::sync::Mutex<Vec<(String, String, OutboundMessage)>>,
        pub deleted: tokio::sync::Mutex<Vec<(String, String)>>,
        pub failing: parking_lot::Mutex<HashMap<String, String>>,
        pub slack_users: parking_lot::Mutex<HashMap<String, String>>,
    }

    impl RecordingSurface {
        /// The text and blocks of everything posted to `channel`.
        pub(crate) async fn posted_to(&self, channel: &str) -> Vec<OutboundMessage> {
            self.sent
                .lock()
                .await
                .iter()
                .filter(|m| m.channel == channel)
                .cloned()
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl ChannelSurface for RecordingSurface {
        fn platform(&self) -> &str {
            "mock"
        }
        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities::default()
        }
        async fn add_reaction(&self, _: &Reaction) -> eyre::Result<()> {
            Ok(())
        }
        async fn remove_reaction(&self, _: &Reaction) -> eyre::Result<()> {
            Ok(())
        }
        async fn send_message(&self, m: &OutboundMessage) -> eyre::Result<SentMessage> {
            // Yields first, so concurrent deliveries interleave as they
            // would across hubs.
            tokio::task::yield_now().await;
            if let Some(error) = self.failing.lock().get(&m.channel).cloned() {
                return Err(eyre!(error));
            }
            let mut sent = self.sent.lock().await;
            sent.push(m.clone());
            Ok(SentMessage {
                channel: m.channel.clone(),
                message_id: format!("m{}", sent.len()),
                thread_id: m.thread_id.clone(),
                platform: "mock".into(),
            })
        }
        async fn edit_message(&self, channel: &str, _: &str, text: &str) -> eyre::Result<()> {
            self.sent.lock().await.push(OutboundMessage {
                channel: channel.to_string(),
                text: text.to_string(),
                thread_id: None,
                username: None,
                icon_url: None,
                blocks: None,
            });
            Ok(())
        }
        async fn update_message(
            &self,
            channel: &str,
            message_id: &str,
            message: &OutboundMessage,
        ) -> eyre::Result<()> {
            self.updated.lock().await.push((
                channel.to_string(),
                message_id.to_string(),
                message.clone(),
            ));
            Ok(())
        }
        async fn delete_message(&self, channel: &str, message_id: &str) -> eyre::Result<()> {
            self.deleted
                .lock()
                .await
                .push((channel.to_string(), message_id.to_string()));
            Ok(())
        }
        async fn open_conversation(&self, user_id: &str) -> eyre::Result<String> {
            Ok(format!("D-{user_id}"))
        }
        async fn lookup_user_id_by_email(&self, email: &str) -> eyre::Result<String> {
            self.slack_users
                .lock()
                .get(email)
                .cloned()
                .ok_or_else(|| eyre!("users_not_found"))
        }
    }

    pub(crate) const WORKFLOW: &str = "release";

    pub(crate) async fn hub() -> Hub {
        let runtime = runtime().await;
        let store = Arc::new(workforce_identity::sqlite::SqliteIdentityStore::in_memory().unwrap());
        let person = |name: &str, role| {
            let identity = Identity::new(name, role);
            workforce_identity::store::IdentityStore::create(store.as_ref(), &identity).unwrap();
            identity.id.to_string()
        };
        let (alice, bob, carol) = (
            person("Alice", IdentityRole::Member),
            person("Bob", IdentityRole::Member),
            person("Carol", IdentityRole::SystemAdmin),
        );
        runtime.service.set_identity_store(store);
        Hub {
            service: Arc::clone(&runtime.service),
            runtime,
            alice,
            bob,
            carol,
        }
    }

    impl Hub {
        /// Alice's workflow as it stands now: who it reaches, and whether it
        /// lets others steer its runs. Returns its key.
        pub(crate) async fn define(&self, visibility: Visibility, by_others: bool) -> String {
            self.define_steps(
                visibility,
                by_others,
                "[[step]]\nid = \"review\"\nname = \"Review\"\ntask = \"check\"\n\
                 pause = \"awaiting-review\"\n\n\
                 [[step]]\nid = \"ship\"\nname = \"Ship\"\ntask = \"ship\"\nexit = true\n",
            )
            .await
        }

        /// Alice's workflow as [`Self::define`] makes it, with `steps`.
        pub(crate) async fn define_steps(
            &self,
            visibility: Visibility,
            by_others: bool,
            steps: &str,
        ) -> String {
            let def = WorkflowDefinition::from_toml(&format!(
                "[workflow]\nid = \"{WORKFLOW}\"\nname = \"Release\"\n\
                 steerable_by_others = {by_others}\n\n{steps}"
            ))
            .unwrap();
            let owned = OwnedDefinition {
                def,
                owner: Some(self.alice.clone()),
                visibility,
                last_editor: Some(self.alice.clone()),
                run_as: None,
                content_hash: None,
            };
            let key = owned.key();
            self.runtime
                .definitions
                .write()
                .await
                .insert(key.clone(), owned);
            key
        }

        /// A run of `key` that `starter` started under its terms of now,
        /// held at its review gate.
        pub(crate) async fn run_of(&self, key: &str, starter: Option<&str>) -> String {
            let owned = self
                .runtime
                .definitions
                .read()
                .await
                .get(key)
                .cloned()
                .unwrap();
            let mut run = WorkflowExecutor::start(
                &owned.def,
                TriggerInfo {
                    source: TriggerSource::Api,
                    payload: serde_json::json!({}),
                },
                None,
            )
            .unwrap();
            run.workflow_id = key.to_string();
            run.status = InstanceStatus::Paused;
            persist_principal(
                &mut run,
                &TurnPrincipal::run_as(starter.map(str::to_string)),
            );
            persist_steering(&mut run, &owned);
            self.runtime.save_instance(&run).unwrap();
            run.id
        }

        /// Alice's workflow whose run goes by itself to a sign-off gate and
        /// waits there for a person: `approved` ships it, `rejected` sends
        /// it to be fixed. No step asks a model.
        pub(crate) async fn define_gate(&self, visibility: Visibility, by_others: bool) -> String {
            self.define_steps(visibility, by_others, &gate_steps("awaiting-sign-off"))
                .await
        }

        /// A run of `key` that `starter` started, gone on by itself to
        /// where it waits.
        pub(crate) async fn run_to_wait(&self, key: &str, starter: Option<&str>) -> String {
            let run = self.run_of(key, starter).await;
            let mut instance = self.runtime.load_instance(&run).unwrap();
            instance.status = InstanceStatus::Running;
            self.runtime.save_instance(&instance).unwrap();
            self.runtime
                .execute_loop(&run, None, None, None)
                .await
                .unwrap();
            assert_eq!(self.status(&run), InstanceStatus::Paused);
            run
        }

        /// The run `id` once a resume has carried it as far as it goes.
        pub(crate) async fn settled(&self, id: &str) -> WorkflowInstance {
            for _ in 0..500 {
                let instance = self.runtime.load_instance(id).unwrap();
                if instance.status != InstanceStatus::Running {
                    return instance;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("the run never stopped moving");
        }

        /// The steps the run `id` has run, in order.
        pub(crate) async fn ran(&self, id: &str) -> Vec<String> {
            self.settled(id)
                .await
                .step_history
                .iter()
                .map(|record| record.step_id.clone())
                .collect()
        }

        pub(crate) fn instance(&self, id: &str) -> WorkflowInstance {
            self.runtime.load_instance(id).unwrap()
        }

        pub(crate) fn status(&self, id: &str) -> InstanceStatus {
            self.runtime.load_instance(id).unwrap().status
        }

        /// The workflow `key` gone, its runs kept.
        pub(crate) async fn forget(&self, key: &str) {
            self.runtime.definitions.write().await.remove(key);
        }

        /// A recording Slack surface every run of this hub posts through.
        pub(crate) fn with_slack(&self) -> Arc<RecordingSurface> {
            let surface = Arc::new(RecordingSurface::default());
            let mut registry = SurfaceRegistry::new();
            registry.register(
                workforce_common::channels::SurfaceKey::workspace("slack", "W1"),
                surface.clone(),
            );
            self.runtime
                .set_surface_registry(Arc::new(tokio::sync::RwLock::new(registry)));
            surface
        }

        /// A run of `key` that `starter` started, delivering to `channel`,
        /// gone on by itself to where it waits on a decision.
        pub(crate) async fn run_to_decision(
            &self,
            key: &str,
            starter: Option<&str>,
            channel: Option<&str>,
        ) -> String {
            let run = self.run_of(key, starter).await;
            let mut instance = self.runtime.load_instance(&run).unwrap();
            if let Some(channel) = channel {
                instance.variables["_channel_id"] = serde_json::json!(channel);
            }
            instance.status = InstanceStatus::Running;
            self.runtime.save_instance(&instance).unwrap();
            self.runtime
                .execute_loop(&run, channel.map(str::to_string), None, None)
                .await
                .unwrap();
            run
        }

        pub(crate) fn admin(&self) -> Steerer {
            Steerer {
                writer: crate::def_write_auth::DefWriter {
                    identity_id: Some(self.carol.clone()),
                    is_admin: true,
                },
                actor: Actor::Person {
                    id: self.carol.clone(),
                },
            }
        }
    }

    /// The steps of [`Hub::define_gate`], its gate paused for `pause`.
    pub(crate) fn gate_steps(pause: &str) -> String {
        format!(
            "[[step]]\nid = \"gate\"\nname = \"Sign off\"\ntask = \"\"\n\
             report = \"Ready for sign-off.\"\nreport_render = \"template\"\n\
             pause = \"{pause}\"\n\
             branches = [{{ when = \"approved\", goto = \"ship\" }}, \
             {{ when = \"rejected\", goto = \"fix\" }}]\n\n\
             [[step]]\nid = \"fix\"\nname = \"Fix\"\ntask = \"\"\n\
             report = \"Fixed.\"\nreport_render = \"template\"\nexit = true\n\n\
             [[step]]\nid = \"ship\"\nname = \"Ship\"\ntask = \"\"\n\
             report = \"Shipped.\"\nreport_render = \"template\"\nexit = true\n"
        )
    }

    /// The steps of a digest workflow: `draft` renders its draft, `review`
    /// waits on a decision `extra` adds to, and `send` sends. `deciders` is the
    /// value of the `approvers` variable the decision reads.
    pub(crate) fn decision_steps(deciders: &str, extra: &str) -> String {
        format!(
            "[[variables]]\nid = \"approvers\"\nvalue = \"{deciders}\"\n\n\
             [[variables]]\nid = \"escalated\"\nvalue = \"no\"\n\n\
             [[step]]\nid = \"draft\"\nname = \"Draft\"\ntask = \"\"\n\
             report = \"Drafted.\"\nreport_render = \"template\"\n\n\
             [[step]]\nid = \"review\"\nname = \"Review the digest\"\ntask = \"\"\n\
             report = \"Today's digest.\"\nreport_render = \"template\"\n\
             pause = \"awaiting-approval\"\n\
             branches = [{{ when = \"send\", goto = \"send\" }}]\n\n\
             [step.decision]\ndeciders = \"{{{{ approvers }}}}\"\n{extra}\n\n\
             [[step.decision.buttons]]\nlabel = \"Send\"\noutcome = \"send\"\nstyle = \"primary\"\ndone = \"Sent\"\n\n\
             [[step.decision.buttons]]\nlabel = \"Redraft\"\noutcome = \"redraft\"\ngoto = \"draft\"\n\n\
             [[step.decision.buttons]]\nlabel = \"Cancel\"\noutcome = \"cancel\"\nstyle = \"danger\"\nend = \"cancel\"\n\n\
             [[step.decision.buttons]]\nlabel = \"Done\"\noutcome = \"done\"\nend = \"complete\"\n\n\
             [[step.decision.buttons]]\nlabel = \"Flag\"\noutcome = \"flag\"\ngoto = \"send\"\nset = {{ escalated = \"yes\" }}\n\n\
             [[step]]\nid = \"send\"\nname = \"Send\"\ntask = \"\"\n\
             report = \"Sent, escalated {{{{ escalated }}}}.\"\nreport_render = \"template\"\nexit = true\n"
        )
    }

    pub(crate) fn member(id: &str) -> Steerer {
        Steerer {
            writer: crate::def_write_auth::DefWriter {
                identity_id: Some(id.to_string()),
                is_admin: false,
            },
            actor: Actor::Person { id: id.to_string() },
        }
    }
}

#[cfg(test)]
mod decision_tests;

#[cfg(test)]
mod steering_tests {
    use super::testing::{hub, member};
    use super::*;
    use crate::run_steering::{SteerAction, SteerRefused};
    use workforce_orchestrator::workflow::instance::Actor;

    #[tokio::test]
    async fn one_of_two_concurrent_resumes_of_a_plain_pause_goes_through() {
        let hub = hub().await;
        let key = hub.define_gate(Visibility::Private, false).await;
        let run = hub.run_to_wait(&key, Some(&hub.alice)).await;
        let by = Steer::hub();

        let (approved, rejected) = tokio::join!(
            hub.runtime
                .resume_workflow(&run, Some("approved".into()), None, None, None, &by),
            hub.runtime
                .resume_workflow(&run, Some(REJECTED.into()), None, None, None, &by),
        );

        assert_eq!(
            [approved.is_ok(), rejected.is_ok()]
                .into_iter()
                .filter(|went| *went)
                .count(),
            1
        );
        let ran = hub.ran(&run).await;
        assert!(ran == ["gate", "ship"] || ran == ["gate", "fix"], "{ran:?}");
    }

    #[tokio::test]
    async fn a_run_keeps_the_steering_terms_it_started_under() {
        let hub = hub().await;
        let key = hub.define(Visibility::Team, false).await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        assert_eq!(
            SteerTerms::kept_in(&hub.runtime.load_instance(&run).unwrap().variables),
            Some(SteerTerms {
                by_others: false,
                visibility: Visibility::Team
            })
        );
    }

    /// A run resumed by hand is the resumer's to decide: it keeps no
    /// suspension hold that a reactivation would lift after a later pause.
    #[tokio::test]
    async fn a_run_resumed_by_hand_keeps_no_suspension_hold() {
        let hub = hub().await;
        let key = hub
            .define_steps(
                Visibility::Private,
                false,
                "[[step]]\nid = \"ship\"\nname = \"Ship\"\ntask = \"\"\n\
                 report = \"Shipped.\"\nreport_render = \"template\"\nexit = true\n",
            )
            .await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        let mut instance = hub.runtime.load_instance(&run).unwrap();
        park_for_suspension(&mut instance);
        hub.runtime.save_instance(&instance).unwrap();

        let by = Steer::granted(&hub.admin(), crate::run_steering::Standing::Admin);
        hub.runtime
            .resume_workflow(&run, None, None, None, None, &by)
            .await
            .unwrap();
        assert!(!held_for_suspension(
            &hub.runtime.load_instance(&run).unwrap().variables
        ));
    }

    #[tokio::test]
    async fn a_resume_routes_a_step_halted_on_a_label_naming_no_branch_and_says_it_went_on_with_it()
    {
        let hub = hub().await;
        let kept = crate::decision_records::testing::record_in_memory();
        let key = hub
            .define_steps(
                Visibility::Private,
                false,
                "[[step]]\nid = \"review\"\nname = \"Review\"\ntask = \"check\"\n\
                 branches = [{ when = \"approved\", goto = \"ship\" }]\n\n\
                 [[step]]\nid = \"ship\"\nname = \"Ship\"\ntask = \"\"\n\
                 report = \"Shipped.\"\nreport_render = \"template\"\nexit = true\n",
            )
            .await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        let def = hub.runtime.definitions.read().await[&key].def.clone();
        let mut instance = hub.runtime.load_instance(&run).unwrap();
        let response = "Looks fine.\nOUTCOME: aproved";
        instance.record_step("review", None, response, Utc::now(), 1, 0.0);
        let halted = WorkflowExecutor::complete_step(&def, &mut instance, response).unwrap();
        assert!(
            matches!(halted, StepTransition::NoRoute { .. }),
            "{halted:?}"
        );
        WorkflowExecutor::fail(&mut instance, "Routing error");
        hub.runtime.save_instance(&instance).unwrap();

        let by = Steer::granted(&hub.admin(), crate::run_steering::Standing::Admin);
        hub.runtime
            .resume_workflow(&run, Some("approved".into()), None, None, None, &by)
            .await
            .unwrap();

        let kept = kept.interventions_of(&run, 1).await;
        assert_eq!(
            kept.iter()
                .map(|i| (
                    i.kind,
                    &i.actor,
                    i.outcome.as_deref(),
                    i.applied,
                    i.to_step.as_deref()
                ))
                .collect::<Vec<_>>(),
            [(
                InterventionKind::Resume,
                &Actor::Person {
                    id: hub.carol.clone()
                },
                Some("approved"),
                Some(true),
                Some("ship")
            )]
        );
        let review = hub.runtime.load_instance(&run).unwrap().step_history[0].clone();
        assert_eq!(
            (review.outcome.as_deref(), review.provenance.outcome_by),
            (Some("approved"), Some(OutcomeBy::Resume))
        );
    }

    /// A run held before its next step runs, for a spent usage window or a
    /// suspension, has no run of that step for an outcome to route.
    #[tokio::test]
    async fn a_resume_with_an_outcome_at_a_held_step_runs_it_and_leaves_the_step_before() {
        let hub = hub().await;
        let kept = crate::decision_records::testing::record_in_memory();
        let key = hub
            .define_steps(
                Visibility::Private,
                false,
                "[[step]]\nid = \"review\"\nname = \"Review\"\ntask = \"check\"\n\
                 branches = [{ when = \"approved\", goto = \"ship\" }, \
                 { when = \"rejected\", goto = \"ship\" }]\n\n\
                 [[step]]\nid = \"ship\"\nname = \"Ship\"\ntask = \"\"\n\
                 report = \"Shipped.\"\nreport_render = \"template\"\nexit = true\n",
            )
            .await;
        let def = hub.runtime.definitions.read().await[&key].def.clone();
        let holds: [fn(&mut WorkflowInstance); 2] = [park_for_suspension, |instance| {
            park_for_usage_cap(instance, "usage window spent")
        }];
        for hold in holds {
            let run = hub.run_of(&key, Some(&hub.alice)).await;
            let mut instance = hub.runtime.load_instance(&run).unwrap();
            let response = "Looks fine.\nOUTCOME: approved";
            instance.record_step("review", None, response, Utc::now(), 1, 0.0);
            WorkflowExecutor::complete_step(&def, &mut instance, response).unwrap();
            assert_eq!(instance.current_step_id.as_deref(), Some("ship"));
            hold(&mut instance);
            hub.runtime.save_instance(&instance).unwrap();

            let by = Steer::granted(&member(&hub.alice), crate::run_steering::Standing::Starter);
            let applied = hub
                .runtime
                .resume_workflow(&run, Some("rejected".into()), None, None, None, &by)
                .await
                .unwrap();

            assert_eq!(applied, Some(false));
            let kept = kept.interventions_of(&run, 1).await;
            assert_eq!(
                kept.iter()
                    .map(|i| (
                        i.kind,
                        i.outcome.as_deref(),
                        i.applied,
                        i.from_step.as_deref(),
                        i.to_step.as_deref()
                    ))
                    .collect::<Vec<_>>(),
                [(
                    InterventionKind::Resume,
                    Some("rejected"),
                    Some(false),
                    Some("ship"),
                    Some("ship")
                )]
            );
            assert_eq!(hub.ran(&run).await, ["review", "ship"]);
            let review = hub.runtime.load_instance(&run).unwrap().step_history[0].clone();
            assert_eq!(
                (review.outcome.as_deref(), review.provenance.outcome_by),
                (Some("approved"), Some(OutcomeBy::OutcomeLine))
            );
        }
    }

    /// A gate waits on its own completed run, so the outcome a person gives
    /// there is set on it and routes it; one the gate has no branch for
    /// leaves it to run and wait again.
    #[tokio::test]
    async fn a_gate_waits_on_its_own_run_and_routes_by_the_persons_outcome() {
        let hub = hub().await;
        let key = hub.define_gate(Visibility::Private, false).await;
        let by = Steer::granted(&member(&hub.alice), crate::run_steering::Standing::Starter);
        for (outcome, applied, ran) in [
            ("approved", Some(true), &["gate", "ship"][..]),
            ("rejected", Some(true), &["gate", "fix"][..]),
            ("maybe", Some(false), &["gate", "gate"][..]),
        ] {
            let run = hub.run_to_wait(&key, Some(&hub.alice)).await;
            let waiting = hub.runtime.load_instance(&run).unwrap();
            assert_eq!(waiting.current_step_id.as_deref(), Some("gate"));
            assert!(waiting.last_record_of("gate").is_some());

            let said = hub
                .runtime
                .resume_workflow(&run, Some(outcome.into()), None, None, None, &by)
                .await
                .unwrap();

            assert_eq!(said, applied, "{outcome}");
            assert_eq!(hub.ran(&run).await, ran, "{outcome}");
            let gate = hub.runtime.load_instance(&run).unwrap().step_history[0].clone();
            assert_eq!(
                (gate.outcome.as_deref(), gate.provenance.outcome_by),
                (Some(outcome), Some(OutcomeBy::Resume)),
                "{outcome}"
            );
        }
    }

    #[test]
    fn a_resume_says_whether_its_step_went_on_with_the_outcome() {
        assert_eq!(
            resumed_message(Some("approved"), Some(true), "*"),
            "Workflow resumed with outcome *approved*."
        );
        assert_eq!(
            resumed_message(Some(REJECTED), Some(false), "'"),
            "Workflow resumed, but its step did not go on with outcome 'rejected'."
        );
        assert_eq!(resumed_message(None, None, "*"), "Workflow resumed.");
    }

    #[tokio::test]
    async fn anyone_signed_in_may_steer_a_run_of_an_operator_workflow() {
        let hub = hub().await;
        let mine = hub.define(Visibility::Private, true).await;
        let def = hub.runtime.definitions.read().await[&mine].def.clone();
        let operator = system_def(def);
        let key = operator.key();
        hub.runtime
            .definitions
            .write()
            .await
            .insert(key.clone(), operator);
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        assert!(hub
            .runtime
            .authorize_steer(&run, &member(&hub.bob), SteerAction::Resume)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn switching_the_flag_off_mid_run_binds_the_run_under_way() {
        let hub = hub().await;
        let key = hub.define(Visibility::Global, true).await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        let bob = member(&hub.bob);
        assert!(hub
            .runtime
            .authorize_steer(&run, &bob, SteerAction::Pause)
            .await
            .is_ok());

        hub.define(Visibility::Global, false).await;
        assert_eq!(
            hub.runtime
                .authorize_steer(&run, &bob, SteerAction::Pause)
                .await,
            Err(SteerRefused::Closed {
                workflow: WORKFLOW_ID.into()
            })
        );
        hub.define(Visibility::Private, true).await;
        assert_eq!(
            hub.runtime
                .authorize_steer(&run, &bob, SteerAction::Pause)
                .await,
            Err(SteerRefused::Private {
                workflow: WORKFLOW_ID.into()
            }),
            "made private mid-run"
        );
    }

    const WORKFLOW_ID: &str = super::testing::WORKFLOW;

    #[tokio::test]
    async fn widening_a_workflow_mid_run_does_not_open_the_run_under_way() {
        let hub = hub().await;
        let key = hub.define(Visibility::Private, false).await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        hub.define(Visibility::Global, true).await;
        assert_eq!(
            hub.runtime
                .authorize_steer(&run, &member(&hub.bob), SteerAction::Abort)
                .await,
            Err(SteerRefused::Private {
                workflow: WORKFLOW_ID.into()
            })
        );
        let later = hub.run_of(&key, Some(&hub.alice)).await;
        assert!(
            hub.runtime
                .authorize_steer(&later, &member(&hub.bob), SteerAction::Abort)
                .await
                .is_ok(),
            "a run started after it was widened"
        );
    }

    #[tokio::test]
    async fn a_run_started_before_terms_were_kept_is_bound_by_todays() {
        let hub = hub().await;
        let key = hub.define(Visibility::Global, true).await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        let mut old = hub.runtime.load_instance(&run).unwrap();
        old.variables.as_object_mut().unwrap().remove(STEERING_KEY);
        hub.runtime.save_instance(&old).unwrap();
        let bob = member(&hub.bob);
        assert!(hub
            .runtime
            .authorize_steer(&run, &bob, SteerAction::Goto)
            .await
            .is_ok());
        hub.define(Visibility::Global, false).await;
        assert!(hub
            .runtime
            .authorize_steer(&run, &bob, SteerAction::Goto)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn the_starter_may_steer_a_run_whose_workflow_is_gone_and_nobody_else_but_an_admin() {
        let hub = hub().await;
        let key = hub.define(Visibility::Global, true).await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        hub.runtime.definitions.write().await.remove(&key);
        assert!(hub
            .runtime
            .authorize_steer(&run, &member(&hub.alice), SteerAction::Abort)
            .await
            .is_ok());
        assert!(hub
            .runtime
            .authorize_steer(&run, &hub.admin(), SteerAction::Abort)
            .await
            .is_ok());
        assert_eq!(
            hub.runtime
                .authorize_steer(&run, &member(&hub.bob), SteerAction::Abort)
                .await,
            Err(SteerRefused::Gone)
        );
    }

    #[tokio::test]
    async fn pausing_and_stopping_a_run_are_kept_with_who_did_it() {
        let hub = hub().await;
        let kept = crate::decision_records::testing::record_in_memory();
        let key = hub.define(Visibility::Global, true).await;
        let run = hub.run_of(&key, Some(&hub.alice)).await;
        let mut running = hub.runtime.load_instance(&run).unwrap();
        running.status = InstanceStatus::Running;
        hub.runtime.save_instance(&running).unwrap();
        let bob = member(&hub.bob);

        let by = hub
            .runtime
            .authorize_steer(&run, &bob, SteerAction::Pause)
            .await
            .unwrap();
        hub.runtime.pause_workflow(&run, &by).await.unwrap();
        hub.runtime.abort_workflow(&run, &by).await.unwrap();

        let kept = kept.interventions_of(&run, 2).await;
        let bob_actor = Actor::Person {
            id: hub.bob.clone(),
        };
        assert_eq!(
            kept.iter()
                .map(|i| (i.kind, &i.actor, i.to_step.as_deref()))
                .collect::<Vec<_>>(),
            [
                (InterventionKind::Pause, &bob_actor, Some("review")),
                (InterventionKind::Abort, &bob_actor, None),
            ]
        );
        assert_eq!(hub.status(&run), InstanceStatus::Cancelled);
        assert_eq!(
            instance_principal(&hub.runtime.load_instance(&run).unwrap())
                .run_as
                .as_deref(),
            Some(hub.alice.as_str()),
            "the run still acts as the person who started it"
        );
    }
}
