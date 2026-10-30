use std::path::PathBuf;
use std::sync::Arc;

use eyre::Result;
use futures::StreamExt;
use tracing::{debug, info, warn};
use workforce_core::types::AgentId;

use crate::notes_tool::NotesTool;
use crate::state::HubState;
use crate::usage_tools::{usage_tools, UsageToolContext};
use workforce_agent::llm::provider::{create_provider, ProviderKind};
use workforce_agent::llm::tiers::{parse_provider_tier_key, TierRegistry};
use workforce_agent::model_router::RoutingConfig;
use workforce_agent::runner::{default_tools, AgentRunner, RunnerConfig, ToolSecrets};
use workforce_common::admin::AuditOutcome;
use workforce_common::cost::{CostEntry, TokenCounts};
use workforce_common::identity::AgentIdentity;
use workforce_common::persistence::{
    RoundTelemetryRecord, TaskTelemetryRecord, ToolTelemetryRecord,
};
use workforce_common::session::TurnPrincipal;
use workforce_common::settings::{AgentConfig, AgentWorkspaceConfig, LlmConfig};
use workforce_common::subscription::Origin;
use workforce_orchestrator::task::{TaskId, TaskResult};

/// A task assignment message sent via NATS.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct TaskAssignment {
    pub task_id: String,
    pub title: String,
    pub description: String,
    pub agent_id: String,
    pub team_id: Option<String>,
    #[serde(default)]
    pub task_type: Option<String>,
    /// The identity that submitted the task, which it runs as.
    #[serde(default)]
    pub requester: Option<String>,
    /// The submitter's billing identity when it differs from `requester`.
    #[serde(default)]
    pub bill_to: Option<String>,
}

impl TaskAssignment {
    fn principal(&self) -> TurnPrincipal {
        TurnPrincipal {
            run_as: self.requester.clone(),
            bill_to: self.bill_to.clone(),
        }
    }
}

/// A task result message sent back via NATS.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct TaskResultMessage {
    pub task_id: String,
    pub success: bool,
    pub response: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    pub duration_ms: u64,
    pub rounds: u32,
    pub model: String,
}

/// Start the agent worker that processes task assignments.
///
/// This runs as a background task, subscribing to task assignment
/// messages and running agents to complete them.
pub async fn run_agent_worker(state: Arc<HubState>) -> Result<()> {
    let nats = match &state.nats {
        Some(n) => n.clone(),
        None => {
            info!("No NATS connection — agent worker running in local mode");
            return Ok(());
        }
    };

    // Subscribe to the task assignment subject
    let subject = "workforce.system.tasks.assign";
    let mut sub = nats
        .queue_subscribe(subject.to_string(), "workforce-hub".into())
        .await?;
    info!("Agent worker listening on {}", subject);

    while let Some(msg) = sub.next().await {
        let state = Arc::clone(&state);
        let nats = nats.clone();

        // Process each task in a separate tokio task
        tokio::spawn(async move {
            match serde_json::from_slice::<TaskAssignment>(&msg.payload) {
                Ok(assignment) => {
                    info!(
                        task_id = %assignment.task_id,
                        agent = %assignment.agent_id,
                        "Processing task assignment"
                    );
                    if let Ok(result) = run_assignment(&state, &assignment).await {
                        let result_subject =
                            format!("workforce.system.tasks.{}.result", assignment.task_id);
                        let result_json = serde_json::to_vec(&result).unwrap_or_default();
                        let _ = nats.publish(result_subject, result_json.into()).await;
                    }
                }
                Err(e) => {
                    warn!("Invalid task assignment message: {}", e);
                }
            }
        });
    }

    Ok(())
}

/// Run `assignment` as the person who submitted it, and settle it: the
/// orchestrator's task, its cost, its telemetry and the audit log.
async fn run_assignment(
    state: &HubState,
    assignment: &TaskAssignment,
) -> Result<TaskResultMessage> {
    crate::user_config::as_principal(&assignment.principal(), async {
        let outcome = execute_task(state, assignment).await;
        match &outcome {
            Ok(result) => settle_completed(state, assignment, result).await,
            Err(e) => settle_failed(state, assignment, e).await,
        }
        outcome
    })
    .await
}

async fn settle_completed(
    state: &HubState,
    assignment: &TaskAssignment,
    result: &TaskResultMessage,
) {
    let task_id = TaskId::from(&assignment.task_id);
    let _ = state
        .orchestrator
        .complete_task(
            &task_id,
            TaskResult {
                success: result.success,
                summary: result.response.clone(),
                output: serde_json::json!({
                    "input_tokens": result.input_tokens,
                    "output_tokens": result.output_tokens,
                }),
                total_tokens: (result.input_tokens + result.output_tokens) as u64,
            },
        )
        .await;

    let identity_id = crate::user_config::current_billing_identity_id();
    let description = workforce_common::text::truncate_with_ellipsis(&assignment.description, 200);
    state.cost_tracker.record(CostEntry {
        team_id: assignment.team_id.as_deref(),
        task_id: Some(&assignment.task_id),
        task_type: assignment.task_type.as_deref(),
        identity_id: identity_id.as_deref(),
        description: Some(&description),
        ..CostEntry::new(
            &assignment.agent_id,
            &result.model,
            TokenCounts {
                input: u64::from(result.input_tokens),
                output: u64::from(result.output_tokens),
                cache_creation: u64::from(result.cache_creation_input_tokens),
                cache_read: u64::from(result.cache_read_input_tokens),
            },
            Origin::ApiTask,
        )
    });

    state.audit_log.record(
        &assignment.agent_id,
        "task.complete",
        &assignment.task_id,
        Some(workforce_common::text::truncate_str(&result.response, 200)),
        if result.success {
            AuditOutcome::Success
        } else {
            AuditOutcome::Error
        },
    );

    info!(
        task_id = %assignment.task_id,
        success = result.success,
        model = %result.model,
        "Task completed"
    );
}

async fn settle_failed(state: &HubState, assignment: &TaskAssignment, e: &eyre::Report) {
    warn!(task_id = %assignment.task_id, error = %e, "Task execution failed");
    let task_id = TaskId::from(&assignment.task_id);
    let _ = state.orchestrator.fail_task(&task_id, &e.to_string()).await;

    let error_str = e.to_string();
    let error_type = if error_str.contains("timeout") {
        "timeout"
    } else if error_str.contains("rate limit") || error_str.contains("429") {
        "rate_limit"
    } else if error_str.contains("API key") || error_str.contains("401") {
        "auth"
    } else {
        "execution"
    };
    let agent_llm = state
        .config
        .agent_by_id(&assignment.agent_id)
        .map(|a| a.effective_llm(&state.config.llm));
    let failed_model = agent_llm
        .as_ref()
        .map(|llm| llm.default_model.clone())
        .unwrap_or_else(|| state.config.llm.default_model.clone());
    let failed_provider = agent_llm
        .map(|llm| llm.provider)
        .unwrap_or_else(|| state.config.llm.provider.clone());
    if let Err(te) = state.telemetry_store.record_task_telemetry(
        &TaskTelemetryRecord {
            agent_id: assignment.agent_id.clone(),
            model: failed_model,
            provider: failed_provider,
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_usd: 0.0,
            rounds: 0,
            duration_ms: 0,
            tool_invocation_count: 0,
            success: false,
            error_type: Some(error_type.to_string()),
            retry_count: 0,
            task_type: assignment.task_type.clone(),
            task_id: Some(assignment.task_id.clone()),
            started_at: chrono::Utc::now().to_rfc3339(),
            completed_at: chrono::Utc::now().to_rfc3339(),
            forecast_cost_usd: None,
            forecast_rounds: None,
            forecast_confidence: None,
            team_id: None,
            session_id: None,
            model_switches: None,
            ticket_id: None,
            identity_id: crate::user_config::current_identity_id(),
            description: Some(workforce_common::text::truncate_with_ellipsis(
                &assignment.description,
                200,
            )),
        },
        &[],
        &[],
    ) {
        warn!("Failed to record failure telemetry: {}", te);
    }

    state.audit_log.record(
        &assignment.agent_id,
        "task.failed",
        &assignment.task_id,
        Some(&error_str),
        AuditOutcome::Error,
    );
}

struct WorkerLlm {
    effective_llm: LlmConfig,
    provider_kind: ProviderKind,
    routing: RoutingConfig,
    tiers: TierRegistry,
}

/// Resolve a background task's LLM config from the agent's own workspace
/// `config.toml` (model / limits / routing / equivalences), falling back to
/// global. A tier-name `default_model` is resolved to a concrete id here since
/// the worker's plain provider does not tier-resolve. `[llm.model_preferences]`
/// is intentionally not applied — its entries can route across providers, which
/// needs the interactive path's multi-provider machinery the worker lacks.
fn resolve_worker_llm(
    agent_config: Option<&AgentConfig>,
    workspace_config: Option<&AgentWorkspaceConfig>,
    global: &LlmConfig,
) -> WorkerLlm {
    let mut effective_llm =
        crate::serve::merged_effective_llm(agent_config, workspace_config, global);
    let workspace_llm = workspace_config.and_then(|ws| ws.llm.as_ref());
    let tiers = TierRegistry::new(workspace_llm.and_then(|llm| llm.equivalences.as_ref()));
    let (provider_kind, tier_key) = match parse_provider_tier_key(&effective_llm.provider) {
        Ok((name, key)) => (
            ProviderKind::from_str_loose(&name).unwrap_or(ProviderKind::OpenRouter),
            key,
        ),
        Err(e) => {
            warn!(provider = %effective_llm.provider, error = %e, "Worker: unrecognized provider; falling back to OpenRouter");
            (ProviderKind::OpenRouter, effective_llm.provider.clone())
        }
    };
    effective_llm.default_model = tiers.resolve_model(&effective_llm.default_model, &tier_key);
    let routing = workspace_llm
        .and_then(|llm| llm.routing.as_ref())
        .map(RoutingConfig::from)
        .unwrap_or_default();
    WorkerLlm {
        effective_llm,
        provider_kind,
        routing,
        tiers,
    }
}

/// Execute a task using the agent runner.
async fn execute_task(state: &HubState, assignment: &TaskAssignment) -> Result<TaskResultMessage> {
    // Mark task as in progress
    let task_id = TaskId::from(&assignment.task_id);
    let _ = state.orchestrator.start_task(&task_id).await;

    // Resolve per-agent LLM config from the agent's own workspace config.toml,
    // falling back to global. The worker previously read only the global
    // [[agents]] merge, so background tasks ignored workspace overrides.
    let agent_config = state.config.agent_by_id(&assignment.agent_id);
    let agent_workspace = agent_config
        .and_then(|a| a.workspace.clone())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(state.config.hub.data_dir.clone())
                .join("agents")
                .join(&assignment.agent_id)
        });
    let workspace_config = AgentWorkspaceConfig::load(&agent_workspace).unwrap_or_else(|e| {
        warn!(agent = %assignment.agent_id, error = %e, "Failed to load workspace config; using global");
        None
    });
    let WorkerLlm {
        effective_llm,
        provider_kind,
        routing,
        tiers,
    } = resolve_worker_llm(agent_config, workspace_config.as_ref(), &state.config.llm);

    let api_key = effective_llm.api_key.as_deref().ok_or_else(|| {
        eyre::eyre!(
            "No LLM API key configured for agent {}",
            assignment.agent_id
        )
    })?;

    let llm = create_provider(
        provider_kind,
        api_key,
        Some(&effective_llm.default_model),
        effective_llm.thinking_budget_tokens,
        effective_llm.zai_subscription,
    )?;

    // Build tools — use the workspace directory so agents can access their files
    let workspace = PathBuf::from(state.config.hub.data_dir.clone()).join("workspace");
    let tools = default_tools(vec![workspace.clone()], ToolSecrets::from_env());
    let agent_id = workforce_core::types::AgentId::new(assignment.agent_id.as_str());
    let sessions = crate::session_store_factory::build_session_store(
        &state.config,
        &workspace,
        agent_id.as_str(),
    )
    .await?;
    tools.register(Arc::new(
        crate::memory_tool::MemoryTool::new(
            Arc::clone(&state.memory),
            agent_id.clone(),
            workspace.clone(),
        )
        .with_sessions(Arc::clone(&sessions)),
    ));
    // Register NotesTool with vector store so write_human indexes into memory (DW-457).
    let recall_scope = agent_config
        .map(|a| a.effective_memory(&state.config.memory).agent_recall_scope)
        .unwrap_or(state.config.memory.agent_recall_scope);
    tools.register(Arc::new(
        NotesTool::with_store(
            workspace.clone(),
            Arc::clone(&state.memory),
            agent_id.clone(),
        )
        .with_sessions(Arc::clone(&sessions))
        .with_recall_scope(recall_scope),
    ));

    // Register usage statistics tools
    let usage_ctx = UsageToolContext {
        store: Arc::clone(&state.telemetry_store),
        agent_id: assignment.agent_id.clone(),
        allowed_models: {
            let mut models = effective_llm.failover.clone();
            models.insert(0, effective_llm.default_model.clone());
            models
        },
    };
    for tool in usage_tools(usage_ctx) {
        tools.register(tool);
    }

    // Register cost analytics tools (DW-340)
    let cost_analytics_ctx = crate::cost_analytics_tools::CostAnalyticsToolContext {
        store: Arc::clone(&state.telemetry_store),
        agent_id: assignment.agent_id.clone(),
    };
    for tool in crate::cost_analytics_tools::cost_analytics_tools(cost_analytics_ctx) {
        tools.register(tool);
    }

    // Register pricing management tools (DW-334)
    let pricing_ctx = crate::pricing_tools::PricingToolContext {
        store: Arc::clone(&state.telemetry_store),
        cost_tracker: Arc::clone(&state.cost_tracker),
        provider: effective_llm.provider.clone(),
    };
    for tool in crate::pricing_tools::pricing_tools(pricing_ctx) {
        tools.register(tool);
    }

    // Load agent identity from workspace for full personality, memory, and context
    let user_tz = state.config.user_timezone();
    let identity_prompt = AgentIdentity::load_with_timezone(&workspace, user_tz)
        .map(|mut id| id.system_prompt())
        .unwrap_or_default();

    // Build system prompt: identity context + task details
    let system_prompt = if identity_prompt.is_empty() {
        format!(
            "You are agent '{}'. Complete the following task.\n\nTask: {}\n\n{}",
            assignment.agent_id, assignment.title, assignment.description
        )
    } else {
        format!(
            "{}\n\n## Current Task Assignment\nAgent: {}\nTask: {}\n\n{}",
            identity_prompt, assignment.agent_id, assignment.title, assignment.description
        )
    };

    let config = RunnerConfig {
        agent_id: AgentId::new(&assignment.agent_id),
        system_prompt,
        model: Some(effective_llm.default_model.clone()),
        max_tokens: Some(effective_llm.max_tokens),
        max_rounds: effective_llm.max_rounds,
        team_id: assignment.team_id.clone(),
        role: None,
        compaction: workforce_common::session::CompactionConfig::default(),
        spill: workforce_common::session::SpillConfig::default(),
        routing,
        tier_registry: tiers,
        model_max_tokens: effective_llm.model_max_tokens.clone(),
        context_eviction_interval: None,
        context_eviction_keep_last: 3,
        trace_writer: None,
        trace_tags: Default::default(),
        permission_writer: state.permission_writer().cloned(),
        surface_allows: None,
        permission_gate: state.permission_gate().cloned(),
        arguments: state.config.tools.validation.mode,
        tool_loading: workforce_agent::tools::loading::ToolLoading::default(),
        programs: None,
        capability_gate: state.capability_gate().cloned(),
        // The distributed worker process never routes approvals or
        // resumes pre-executed actions — both are hub-orchestrated.
        approval_router: None,
        preexecute: None,
        journal: None,
        collect: Vec::new(),
        seeds: None,
        interactive: false,
        self_serve: workforce_agent::tools::approval::SelfServe::Answerable,
        compile_gate: None,
    };

    let runner = AgentRunner::new(
        config,
        Arc::from(llm),
        Arc::new(tools),
        Arc::clone(&state.policy_engine),
    );

    // Generate cost forecast before execution
    let forecaster = crate::cost_forecast::CostForecaster::new(
        Arc::clone(&state.telemetry_store),
        state.config.budget.warn_turn_usd,
    );
    let forecast = forecaster
        .forecast(
            &assignment.agent_id,
            assignment.task_type.as_deref(),
            &effective_llm.default_model,
        )
        .await?;

    debug!(
        "Cost forecast for task '{}': ${:.3} (confidence: {:.0}%, basis: {})",
        assignment.title,
        forecast.estimated_cost_usd,
        forecast.confidence * 100.0,
        forecast.basis
    );

    if forecast.warn {
        warn!(
            "Task '{}' predicted to cost ${:.3} (above ${:.2} threshold). Proceeding anyway.",
            assignment.title, forecast.estimated_cost_usd, state.config.budget.warn_turn_usd
        );
    }

    let started_at = chrono::Utc::now();
    let result = runner.run(&assignment.description).await?;
    let completed_at = chrono::Utc::now();

    // Record full telemetry
    let cost_usd = state.cost_tracker.calculate_cost(
        &result.model,
        result.total_usage.input_tokens as u64,
        result.total_usage.output_tokens as u64,
        result.total_usage.cache_creation_input_tokens as u64,
        result.total_usage.cache_read_input_tokens as u64,
    );
    let round_records: Vec<RoundTelemetryRecord> = result
        .round_details
        .iter()
        .map(|rd| RoundTelemetryRecord {
            round_number: rd.round_number,
            input_tokens: rd.usage.input_tokens as u64,
            output_tokens: rd.usage.output_tokens as u64,
            latency_ms: rd.latency_ms,
            cache_creation_tokens: rd.usage.cache_creation_input_tokens as u64,
            cache_read_tokens: rd.usage.cache_read_input_tokens as u64,
            prompt_size_bytes: rd.prompt_size_bytes as u64,
            response_size_bytes: rd.response_size_bytes as u64,
            tool_count: rd.tool_count,
            stop_reason: rd.stop_reason.clone(),
        })
        .collect();
    let tool_records: Vec<ToolTelemetryRecord> = result
        .tool_invocations
        .iter()
        .map(|inv| ToolTelemetryRecord {
            round_number: 0,
            tool_name: inv.tool_name.clone(),
            success: inv.success,
            duration_ms: inv.duration_ms,
            policy_decision: inv.policy_decision.clone(),
            input_size_bytes: serde_json::to_string(&inv.input)
                .map(|s| s.len() as u64)
                .unwrap_or(0),
            output_size_bytes: serde_json::to_string(&inv.output)
                .map(|s| s.len() as u64)
                .unwrap_or(0),
            denial_reason: inv.denial_reason.clone(),
        })
        .collect();

    if let Err(e) = state.telemetry_store.record_task_telemetry(
        &TaskTelemetryRecord {
            agent_id: assignment.agent_id.clone(),
            model: result.model.clone(),
            provider: effective_llm.provider.clone(),
            input_tokens: result.total_usage.input_tokens as u64,
            output_tokens: result.total_usage.output_tokens as u64,
            cache_creation_input_tokens: result.total_usage.cache_creation_input_tokens as u64,
            cache_read_input_tokens: result.total_usage.cache_read_input_tokens as u64,
            cost_usd,
            rounds: result.rounds,
            duration_ms: result.duration_ms,
            tool_invocation_count: result.tool_invocations.len() as u32,
            success: true,
            error_type: None,
            retry_count: 0,
            task_type: assignment.task_type.clone(),
            task_id: Some(assignment.task_id.clone()),
            started_at: started_at.to_rfc3339(),
            completed_at: completed_at.to_rfc3339(),
            // Include forecast for accuracy tracking (DW-249)
            forecast_cost_usd: Some(forecast.estimated_cost_usd),
            forecast_rounds: Some(forecast.estimated_rounds),
            forecast_confidence: Some(forecast.confidence),
            team_id: None,
            session_id: None,
            model_switches: None,
            ticket_id: None,
            identity_id: crate::user_config::current_identity_id(),
            description: Some(workforce_common::text::truncate_with_ellipsis(
                &assignment.description,
                200,
            )),
        },
        &round_records,
        &tool_records,
    ) {
        warn!("Failed to record task telemetry: {}", e);
    }

    Ok(TaskResultMessage {
        task_id: assignment.task_id.clone(),
        success: true,
        response: result.response,
        input_tokens: result.total_usage.input_tokens,
        output_tokens: result.total_usage.output_tokens,
        cache_creation_input_tokens: result.total_usage.cache_creation_input_tokens,
        cache_read_input_tokens: result.total_usage.cache_read_input_tokens,
        duration_ms: result.duration_ms,
        rounds: result.rounds,
        model: result.model,
    })
}

/// Submit a task through the hub and wait for completion.
///
/// This is the high-level API for submitting work to the workforce.
pub async fn submit_task(
    state: &HubState,
    title: &str,
    description: &str,
    team_id: Option<&str>,
    principal: TurnPrincipal,
) -> Result<TaskResultMessage> {
    use workforce_orchestrator::task::Task;

    // Create the task
    let mut task = Task::new(title, description);
    task.team_id = team_id.map(String::from);

    let task_id = state.orchestrator.submit_task(task).await;

    let assignment = TaskAssignment {
        task_id: task_id.0.clone(),
        title: title.to_string(),
        description: description.to_string(),
        agent_id: "default".to_string(),
        team_id: team_id.map(String::from),
        task_type: Some("api_task".to_string()),
        requester: principal.run_as,
        bill_to: principal.bill_to,
    };

    // If NATS is connected, publish assignment
    if let Some(ref nats) = state.nats {
        // Subscribe to result subject before publishing to avoid race
        let result_subject = format!("workforce.system.tasks.{}.result", task_id);
        let mut result_sub = nats.subscribe(result_subject.clone()).await?;

        let payload = serde_json::to_vec(&assignment)?;
        nats.publish("workforce.system.tasks.assign".to_string(), payload.into())
            .await?;
        info!(task_id = %task_id, "Task published to NATS");

        // Wait for the result with a 5-minute timeout
        match tokio::time::timeout(std::time::Duration::from_secs(300), result_sub.next()).await {
            Ok(Some(msg)) => {
                let _ = result_sub.unsubscribe().await;
                match serde_json::from_slice::<TaskResultMessage>(&msg.payload) {
                    Ok(result) => Ok(result),
                    Err(e) => {
                        warn!(task_id = %task_id, "Failed to parse task result: {}", e);
                        Ok(TaskResultMessage {
                            task_id: task_id.0,
                            success: false,
                            response: format!(
                                "Task completed but result could not be parsed: {}",
                                e
                            ),
                            input_tokens: 0,
                            output_tokens: 0,
                            cache_creation_input_tokens: 0,
                            cache_read_input_tokens: 0,
                            duration_ms: 0,
                            rounds: 0,
                            model: String::new(),
                        })
                    }
                }
            }
            Ok(None) => Ok(TaskResultMessage {
                task_id: task_id.0,
                success: false,
                response: "NATS subscription closed before result received".to_string(),
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                duration_ms: 0,
                rounds: 0,
                model: String::new(),
            }),
            Err(_) => {
                let _ = result_sub.unsubscribe().await;
                Ok(TaskResultMessage {
                    task_id: task_id.0,
                    success: false,
                    response: "Task timed out after 300s — check /api/tasks for status".to_string(),
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    duration_ms: 0,
                    rounds: 0,
                    model: String::new(),
                })
            }
        }
    } else {
        run_assignment(state, &assignment).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use workforce_common::settings::{AgentLlmConfig, WorkspaceRoutingConfig};

    /// A hub with no LLM key, so a task fails at once and settles offline.
    fn keyless_hub(data_dir: &std::path::Path) -> HubState {
        let mut config = workforce_common::settings::WorkforceConfig::default();
        config.hub.data_dir = data_dir.to_string_lossy().to_string();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        HubState::for_tests(config)
    }

    #[tokio::test]
    async fn nats_task_assignment_runs_as_requester() {
        const REQUESTER: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
        let tmp = tempfile::tempdir().unwrap();
        let state = keyless_hub(tmp.path());
        let published = |task_id: &str, requester: Option<&str>| {
            let mut message = serde_json::json!({
                "task_id": task_id,
                "title": "t",
                "description": "d",
                "agent_id": "default",
                "team_id": null,
                "task_type": "api_task",
            });
            if let Some(requester) = requester {
                message["requester"] = requester.into();
            }
            serde_json::from_slice::<TaskAssignment>(&serde_json::to_vec(&message).unwrap())
                .unwrap()
        };

        for (task, requester) in [("task-new", Some(REQUESTER)), ("task-old", None)] {
            assert!(
                run_assignment(&state, &published(task, requester))
                    .await
                    .is_err(),
                "no LLM key"
            );
        }
        let settled: HashMap<Option<String>, Option<String>> = state
            .telemetry_store
            .get_task_history("default", 10, None, None)
            .unwrap()
            .into_iter()
            .map(|row| (row.task_id, row.identity_id))
            .collect();
        assert_eq!(
            settled.get(&Some("task-new".into())),
            Some(&Some(REQUESTER.to_string())),
            "the task runs as the person who submitted it"
        );
        assert_eq!(
            settled.get(&Some("task-old".into())),
            Some(&None),
            "an older publisher's task runs as nobody, as before"
        );
    }

    fn global_llm() -> LlmConfig {
        LlmConfig::default()
    }

    fn workspace(llm: AgentLlmConfig) -> AgentWorkspaceConfig {
        AgentWorkspaceConfig {
            llm: Some(llm),
            ..Default::default()
        }
    }

    #[test]
    fn applies_workspace_model_override() {
        let mut global = global_llm();
        global.provider = "anthropic".into();
        global.default_model = "global-model".into();
        let ws = workspace(AgentLlmConfig {
            default_model: Some("claude-opus-4-7".into()),
            ..Default::default()
        });
        let resolved = resolve_worker_llm(None, Some(&ws), &global);
        assert_eq!(resolved.effective_llm.default_model, "claude-opus-4-7");
    }

    #[test]
    fn applies_workspace_routing() {
        let mut global = global_llm();
        global.provider = "anthropic".into();
        let ws = workspace(AgentLlmConfig {
            routing: Some(WorkspaceRoutingConfig {
                enable_adaptive_routing: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        let resolved = resolve_worker_llm(None, Some(&ws), &global);
        assert!(resolved.routing.enable_adaptive_routing);
    }

    #[test]
    fn resolves_tier_name_default_model_via_equivalences() {
        let mut global = global_llm();
        global.provider = "grok".into();
        global.default_model = "powerful".into();
        let ws = workspace(AgentLlmConfig {
            equivalences: Some(HashMap::from([(
                "powerful".to_string(),
                vec!["grok:grok-from-ws".to_string()],
            )])),
            ..Default::default()
        });
        let resolved = resolve_worker_llm(None, Some(&ws), &global);
        assert_eq!(resolved.effective_llm.default_model, "grok-from-ws");
    }

    #[test]
    fn falls_back_to_global_without_workspace() {
        let mut global = global_llm();
        global.provider = "anthropic".into();
        global.default_model = "global-model".into();
        let resolved = resolve_worker_llm(None, None, &global);
        assert_eq!(resolved.effective_llm.default_model, "global-model");
    }

    #[test]
    fn unrecognized_provider_falls_back_to_openrouter() {
        let mut global = global_llm();
        global.provider = "vertex-anthropic".into();
        let resolved = resolve_worker_llm(None, None, &global);
        assert!(matches!(resolved.provider_kind, ProviderKind::OpenRouter));
    }
}
