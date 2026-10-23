//! What runs a coding round.
//!
//! A [`CodingSandbox`] says where a round runs; a [`CodingEngine`] says what
//! runs it. Every coding surface (`coding.session`, `coding.fanout`, the
//! workflow `claude-code` step) builds a [`CodingRound`] and hands it to the
//! engine, which owns the prompt contract, the model call, the result shape
//! and the cost record. Surfaces keep the conversation: session ids, rotation,
//! delivery.

mod agent_loop;
mod bedrock;
mod claude_code;
pub mod codex;
mod fallback;
mod sandbox_tools;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_agent::runner::ProgressEvent;
use workforce_common::persistence::TaskTelemetryRecord;
use workforce_common::settings::{
    CodingCodexConfig, CodingEngineConfig, CodingEngineKind, CodingSessionConfig, EngineEntry,
    FallbackTrigger,
};

use workforce_common::subscription::SubscriptionKind;

use crate::agent_turn::connect::no_login_notice;
use crate::claude_code::{headline, is_cancelled, CliHealth};
use crate::coding_sandbox::CodingSandbox;
use crate::mcp_gate::RouteResume;
use crate::mcp_turn::{ClientTimeout, McpExposure, RoundTools};
use crate::serve::AgentService;
use crate::subscription::Login;

pub use agent_loop::AgentLoopEngine;
pub use bedrock::{ClaudeCodeBedrockEngine, CredentialMinter, StsMinter};
pub(crate) use claude_code::vision_model as claude_vision_model;
pub use claude_code::ClaudeCodeEngine;
pub use codex::{CodexAuth, CodexEngine, CodexPolicy};
pub use fallback::{attempt_of, handed_over, with_handed_over, FallbackEngine};
pub use sandbox_tools::sandbox_tools;

/// How what an engine spent reaches a bill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Billing {
    /// Charged per token: Bedrock, an API key, the hub's own agent loop.
    Billed,
    /// Priced at list rates on a subscription that charges nothing per token.
    Estimated,
}

/// The billing of a step run on the engine a record names. An engine this
/// does not know counts as billed, so its spend is never left out of a bill.
pub fn billing_of(engine: &str) -> Billing {
    match engine {
        claude_code::LABEL | codex::LABEL | crate::claude_pilot::ENGINE => Billing::Estimated,
        _ => Billing::Billed,
    }
}

/// The names a caller may pick a round's model and effort from, as the engine
/// spells them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoices {
    pub models: Vec<String>,
    pub efforts: &'static [&'static str],
    /// Prefixes of the full model ids the engine passes through besides its
    /// aliases; empty when it takes aliases only, and an engine that lists no
    /// aliases takes any model.
    pub raw_id_prefixes: &'static [&'static str],
}

impl ModelChoices {
    pub fn accepts_model(&self, model: &str) -> bool {
        self.models.is_empty()
            || self.models.iter().any(|m| m.eq_ignore_ascii_case(model))
            || self.raw_id_prefixes.iter().any(|p| model.starts_with(p))
    }

    pub fn accepts_effort(&self, effort: &str) -> bool {
        self.efforts.iter().any(|e| e.eq_ignore_ascii_case(effort))
    }
}

/// Why an engine cannot serve the caller right now, fit to show them, and
/// what moves a chain of engines past it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: String,
    pub trigger: FallbackTrigger,
}

impl Refusal {
    /// The caller has nothing this engine runs on.
    pub fn no_token(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            trigger: FallbackTrigger::NoToken,
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// The result of a CLI round, whichever CLI ran it.
pub type CliRoundResult = crate::claude_code::ClaudeCodeResult;

/// How a round relates to the rounds before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continuity {
    /// No transcript is kept.
    OneShot,
    /// The round continues (`resume`) or starts the transcript at `id`.
    Session { id: Uuid, resume: bool },
}

/// Which of the workspace's own guidance a round loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryLoad {
    pub claude_md: bool,
    pub auto_memory: bool,
}

/// One unit of coding work, independent of the engine that runs it.
#[derive(Debug, Clone)]
pub struct CodingRound {
    pub task: String,
    pub max_turns: Option<u32>,
    /// Wall clock for the whole round across its retries.
    pub timeout: Duration,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The engine's own tools the round may use; hub tools served over MCP are
    /// added from the sandbox.
    pub tools: Vec<String>,
    pub system_prompt: Option<String>,
    pub continuity: Continuity,
    pub memory: MemoryLoad,
    pub gh_token: Option<String>,
    pub cache_dir: Option<PathBuf>,
    /// The durable jobs of a round that can be collected after a restart.
    pub jobs: Option<crate::claude_code::RoundJobs>,
}

impl CodingRound {
    /// Whether the round collects a job already running, after a restart.
    pub fn collecting(&self) -> bool {
        self.jobs
            .as_ref()
            .is_some_and(crate::claude_code::RoundJobs::pinned)
    }

    /// A round with no session, guidance or cache, the shape a workflow step
    /// or a fan-out coder starts from.
    pub fn one_shot(task: String, timeout: Duration) -> Self {
        Self {
            task,
            max_turns: None,
            timeout,
            model: None,
            effort: None,
            tools: Vec::new(),
            system_prompt: None,
            continuity: Continuity::OneShot,
            memory: MemoryLoad {
                claude_md: false,
                auto_memory: false,
            },
            gh_token: None,
            cache_dir: None,
            jobs: None,
        }
    }
}

/// Whose round it is and how to account for it.
pub struct RoundContext<'a> {
    pub service: &'a AgentService,
    pub identity: Option<&'a str>,
    /// Who the round's hub tools run as when no identity is known: the
    /// automated source it runs for.
    pub stand_in: &'a str,
    pub ticket_id: Option<&'a str>,
    /// Names the surface in cost reports, e.g. `coding.session`.
    pub description: &'a str,
    /// Where an approval the round's calls route to designated approvers is
    /// resumed once they decide: the runner turn that started the round, or
    /// nowhere, when nothing could resume it.
    pub resume: RouteResume,
}

/// The ledger key for a Claude Code round: the engine's label, then the
/// model the CLI was asked for.
pub fn labelled_model(engine: &str, model: Option<&str>) -> String {
    model
        .map(|m| format!("{engine}:{m}"))
        .unwrap_or_else(|| engine.to_string())
}

impl RoundContext<'_> {
    /// Who the round's hub tools run as: never nobody, since the capability
    /// and cascade gates pass a call nobody is scoped for.
    pub fn principal(&self) -> &str {
        self.identity.unwrap_or(self.stand_in)
    }

    /// The `kind` login the round's payer runs it on, as the agent's
    /// subscription logins resolve for whoever is in scope; else how they
    /// connect one, or why none serves — a login at its limit moving a chain
    /// on as a limit does.
    pub fn login(&self, kind: SubscriptionKind) -> Result<Login, Refusal> {
        self.service
            .logins
            .resolve(kind)
            .map_err(|no_login| Refusal {
                trigger: match no_login.cooling_until() {
                    Some(_) => FallbackTrigger::RateLimited,
                    None => FallbackTrigger::NoToken,
                },
                reason: no_login_notice(self.service.agent_id.as_str(), &no_login),
            })
    }

    /// The hub tools `sandbox` lets a round call, bound for this round in the
    /// scope of the turn it runs in; `None` when it may call none.
    pub fn round_tools(
        &self,
        sandbox: &dyn CodingSandbox,
        cancel: Option<&CancellationToken>,
        timeout: ClientTimeout,
    ) -> Option<RoundTools> {
        let patterns = sandbox.hub_tool_patterns();
        if patterns.is_empty() {
            return None;
        }
        RoundTools::bind(
            self.service,
            self.principal(),
            McpExposure::Patterns(patterns.into()),
            cancel,
            timeout,
        )
        .map(|tools| tools.resumed(self.resume))
    }

    /// Record what a round cost against the agent: `ledger_model` is the key
    /// the cost ledger prices it under, `engine` the provider it is filed as,
    /// and `served` the subscription that paid for it, if one did.
    pub fn record(
        &self,
        engine: &str,
        ledger_model: &str,
        result: &CliRoundResult,
        started_at: chrono::DateTime<chrono::Utc>,
        served: Option<workforce_common::subscription::Served>,
    ) {
        self.service
            .cost_tracker
            .record(workforce_common::cost::CostEntry {
                task_type: Some("coding"),
                ticket_id: self.ticket_id,
                identity_id: self.identity,
                description: Some(self.description),
                served,
                ledger_key: result.job.as_ref().map(|job| job.as_str()),
                ..workforce_common::cost::CostEntry::new(
                    self.service.agent_id.as_str(),
                    ledger_model,
                    workforce_common::cost::TokenCounts {
                        input: result.input_tokens,
                        output: result.output_tokens,
                        cache_creation: result.cache_creation_tokens,
                        cache_read: result.cache_read_tokens,
                    },
                    workforce_common::subscription::Origin::Coding,
                )
            });
        let success = result.exit_code == 0;
        self.save_telemetry(TaskTelemetryRecord {
            input_tokens: result.input_tokens,
            output_tokens: result.output_tokens,
            cache_creation_input_tokens: result.cache_creation_tokens,
            cache_read_input_tokens: result.cache_read_tokens,
            cost_usd: result.cost_usd,
            rounds: result.num_turns,
            duration_ms: result.duration.as_millis() as u64,
            success,
            error_type: (!success).then(|| format!("exit_{}", result.exit_code)),
            ..self.telemetry_base(engine, ledger_model, started_at)
        });
    }

    /// Log a round `engine` ran that ended in an error, and count it in task
    /// telemetry. A cancelled round is neither: the caller stopped it. What it
    /// spent is unknown, so the cost ledger is left alone rather than shown a
    /// free round.
    pub fn record_error(
        &self,
        engine: &dyn CodingEngine,
        round: &CodingRound,
        started_at: chrono::DateTime<chrono::Utc>,
        error: &eyre::Report,
    ) {
        if is_cancelled(error) {
            return;
        }
        let record = self.failure_record(engine, round, started_at);
        tracing::warn!(
            engine = %record.provider,
            model = %record.model,
            error = %headline(&format!("{error:#}")).unwrap_or_default(),
            "coding round failed"
        );
        self.save_telemetry(record);
    }

    /// The telemetry row for a round `engine` ran that ended in an error,
    /// under the ledger key a finished round on it would have carried.
    fn failure_record(
        &self,
        engine: &dyn CodingEngine,
        round: &CodingRound,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> TaskTelemetryRecord {
        let label = engine.label();
        let ledger_model = labelled_model(
            label,
            engine.model_for(round.model.as_deref(), self).as_deref(),
        );
        TaskTelemetryRecord {
            success: false,
            error_type: Some("error".to_string()),
            ..self.telemetry_base(label, &ledger_model, started_at)
        }
    }

    /// The telemetry row a coding round starts from: whose round it was, what
    /// ran it and for how long, with nothing spent.
    fn telemetry_base(
        &self,
        engine: &str,
        ledger_model: &str,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> TaskTelemetryRecord {
        let completed_at = chrono::Utc::now();
        TaskTelemetryRecord {
            agent_id: self.service.agent_id.to_string(),
            model: ledger_model.to_string(),
            provider: engine.to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_usd: 0.0,
            rounds: 0,
            duration_ms: (completed_at - started_at).num_milliseconds().max(0) as u64,
            tool_invocation_count: 0,
            success: false,
            error_type: None,
            retry_count: 0,
            task_type: Some("coding".to_string()),
            task_id: None,
            started_at: started_at.to_rfc3339(),
            completed_at: completed_at.to_rfc3339(),
            forecast_cost_usd: None,
            forecast_rounds: None,
            forecast_confidence: None,
            team_id: None,
            session_id: None,
            model_switches: None,
            ticket_id: self.ticket_id.map(str::to_string),
            identity_id: self.identity.map(str::to_string),
            description: Some(self.description.to_string()),
        }
    }

    fn save_telemetry(&self, record: TaskTelemetryRecord) {
        if let Err(e) = self
            .service
            .telemetry_store
            .record_task_telemetry(&record, &[], &[])
        {
            tracing::warn!(error = %e, engine = record.provider, "Failed to record coding round telemetry");
        }
    }
}

#[async_trait]
pub trait CodingEngine: Send + Sync {
    /// The name cost reports and results carry for rounds this engine ran.
    fn label(&self) -> &'static str;

    /// Whether a round on this engine bills per token.
    fn metered(&self) -> bool {
        false
    }

    /// Why this engine cannot serve the caller right now, before any
    /// workspace is touched.
    fn preflight(&self, ctx: &RoundContext<'_>) -> Result<(), Refusal>;

    /// Whether `text`, a failed round's account of itself, says the engine's
    /// usage or rate limit is spent, so a chain should move on rather than
    /// retry.
    fn limit_reached(&self, _text: &str) -> bool {
        false
    }

    /// The model and effort names a caller may choose between.
    fn model_choices(&self) -> ModelChoices;

    /// The model a round asking for `requested` runs on, as results and
    /// ledgers name it; `None` when the round names none or cannot run.
    fn model_for(&self, requested: Option<&str>, _ctx: &RoundContext<'_>) -> Option<String> {
        requested.map(str::to_string)
    }

    /// The health of the CLI this engine spawns on this host; `None` for an
    /// engine that spawns none.
    async fn cli_health(&self) -> Option<CliHealth> {
        None
    }

    async fn run(
        &self,
        sandbox: Arc<dyn CodingSandbox>,
        round: &CodingRound,
        ctx: &RoundContext<'_>,
        cancel: Option<CancellationToken>,
        progress: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) -> Result<CliRoundResult>;
}

/// The engines this hub can run a round on, and the chains built from them.
pub struct Engines {
    claude_code: Arc<dyn CodingEngine>,
    claude_code_bedrock: Arc<dyn CodingEngine>,
    codex: Arc<dyn CodingEngine>,
    codex_api: Arc<dyn CodingEngine>,
    model: Arc<dyn CodingEngine>,
}

impl Default for Engines {
    fn default() -> Self {
        Self::new(
            ClaudeCodeBedrockEngine::default(),
            CodingCodexConfig::default(),
            None,
        )
    }
}

impl Engines {
    /// The engines `[coding_session]` describes, for a hub whose data
    /// directory is `data_dir`.
    pub fn from_config(config: &CodingSessionConfig, data_dir: &Path) -> Self {
        Self::new(
            ClaudeCodeBedrockEngine::from_config(&config.bedrock),
            config.codex.clone(),
            Some(data_dir),
        )
    }

    /// `data_dir` is where Codex keeps a local session's home between turns;
    /// `None` keeps it under this host's temp dir.
    pub fn new(
        bedrock: ClaudeCodeBedrockEngine,
        codex: CodingCodexConfig,
        data_dir: Option<&Path>,
    ) -> Self {
        let codex_on = |auth| {
            let engine = CodexEngine::new(auth, codex.clone());
            match data_dir {
                Some(dir) => engine.with_data_dir(dir),
                None => engine,
            }
        };
        Self {
            claude_code: Arc::new(ClaudeCodeEngine),
            claude_code_bedrock: Arc::new(bedrock),
            codex: Arc::new(codex_on(CodexAuth::Subscription)),
            codex_api: Arc::new(codex_on(CodexAuth::ApiKey)),
            model: Arc::new(AgentLoopEngine),
        }
    }

    /// Every kind served by `engine`.
    #[cfg(test)]
    pub(crate) fn only(engine: Arc<dyn CodingEngine>) -> Self {
        Self {
            claude_code: Arc::clone(&engine),
            claude_code_bedrock: Arc::clone(&engine),
            codex: Arc::clone(&engine),
            codex_api: Arc::clone(&engine),
            model: engine,
        }
    }

    pub fn get(&self, kind: CodingEngineKind) -> Arc<dyn CodingEngine> {
        match kind {
            CodingEngineKind::ClaudeCode => Arc::clone(&self.claude_code),
            CodingEngineKind::ClaudeCodeBedrock => Arc::clone(&self.claude_code_bedrock),
            CodingEngineKind::Codex => Arc::clone(&self.codex),
            CodingEngineKind::CodexApi => Arc::clone(&self.codex_api),
            CodingEngineKind::Model => Arc::clone(&self.model),
        }
    }

    /// The engine `config` describes: its primary alone when it has no
    /// fallback, else the chain. `primary` replaces the config's primary
    /// (a workflow step naming its own engine) and `fallback` its chain (the
    /// step's own list) when given.
    pub fn configured(
        &self,
        config: &CodingEngineConfig,
        primary: Option<CodingEngineKind>,
        fallback: Option<&[String]>,
    ) -> Result<Arc<dyn CodingEngine>> {
        let primary = self.get(primary.unwrap_or(config.kind));
        let entries = EngineEntry::parse_all(fallback.unwrap_or(&config.fallback))?;
        if entries.is_empty() {
            return Ok(primary);
        }
        let chain = std::iter::once((primary, None))
            .chain(entries.into_iter().map(|e| (self.get(e.kind), e.model)))
            .collect();
        Ok(Arc::new(FallbackEngine::new(
            chain,
            config.allow_metered,
            config.fallback_on.clone(),
        )))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mcp_turn::testing::service;

    fn context(service: &AgentService) -> RoundContext<'_> {
        RoundContext {
            service,
            identity: Some("user-1"),
            stand_in: workforce_common::subscription::SCHEDULER,
            ticket_id: Some("ENG-1"),
            description: "coding.session",
            resume: crate::mcp_gate::RouteResume::Nowhere,
        }
    }

    fn fable_round() -> CodingRound {
        CodingRound {
            model: Some("fable".into()),
            ..CodingRound::one_shot("fix it".into(), Duration::from_secs(60))
        }
    }

    fn history(service: &AgentService) -> Vec<workforce_common::persistence::TaskHistoryEntry> {
        service
            .telemetry_store
            .get_task_history(service.agent_id.as_str(), 10, None, None)
            .unwrap()
    }

    #[tokio::test]
    async fn failure_telemetry_counts_a_failed_round_without_usage() {
        let service = service().await;
        let ctx = context(&service);
        let started_at = chrono::Utc::now();

        let record = ctx.failure_record(&ClaudeCodeEngine, &fable_round(), started_at);
        assert!(!record.success);
        assert_eq!(record.error_type.as_deref(), Some("error"));
        assert_eq!(
            (
                record.input_tokens,
                record.output_tokens,
                record.cache_creation_input_tokens,
                record.cache_read_input_tokens,
                record.rounds,
            ),
            (0, 0, 0, 0, 0)
        );
        assert_eq!(record.cost_usd, 0.0);
        assert_eq!(record.provider, "claude-code");
        assert_eq!(record.model, "claude-code:fable");
        assert_eq!(record.ticket_id.as_deref(), Some("ENG-1"));
        assert_eq!(record.identity_id.as_deref(), Some("user-1"));

        ctx.record_error(
            &ClaudeCodeEngine,
            &fable_round(),
            started_at,
            &eyre::eyre!("output consumer dropped").wrap_err("secure command runner unavailable"),
        );
        let rows = history(&service);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].success);
        assert_eq!(rows[0].model, "claude-code:fable");
        assert_eq!((rows[0].input_tokens, rows[0].cost_usd), (0, 0.0));
    }

    #[tokio::test]
    async fn a_cancelled_round_records_nothing() {
        let service = service().await;
        context(&service).record_error(
            &ClaudeCodeEngine,
            &fable_round(),
            chrono::Utc::now(),
            &eyre::eyre!("claude cancelled"),
        );
        assert!(history(&service).is_empty());
    }

    pub(crate) fn cost_rows(
        service: &AgentService,
    ) -> Vec<workforce_common::persistence::CostDetailRow> {
        let now = chrono::Utc::now();
        service
            .cost_tracker
            .telemetry_store()
            .unwrap()
            .cost_detail_records(
                now - chrono::Duration::hours(1),
                now + chrono::Duration::hours(1),
                workforce_common::persistence::CostDetailFilter {
                    scope: workforce_common::persistence::CostScope {
                        agent_id: Some(service.agent_id.as_str()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                100,
                0,
            )
            .unwrap()
            .records
    }

    #[tokio::test]
    async fn collected_round_records_cost_once() {
        let service = service().await;
        let ctx = context(&service);
        let jobs = crate::claude_code::RoundJobs::new(
            uuid::Uuid::new_v4().to_string(),
            chrono::Utc::now(),
        );
        let round = |job| CliRoundResult {
            input_tokens: 1_000,
            output_tokens: 100,
            job,
            ..CliRoundResult::default()
        };
        let finished = round(Some(jobs.job()));
        ctx.record(
            "claude-code",
            "claude-code:sonnet",
            &finished,
            chrono::Utc::now(),
            None,
        );
        ctx.record(
            "claude-code",
            "claude-code:sonnet",
            &finished,
            chrono::Utc::now(),
            None,
        );
        assert_eq!(
            cost_rows(&service).len(),
            1,
            "a job collected again after a restart is recorded once"
        );
        ctx.record(
            "claude-code",
            "claude-code:sonnet",
            &round(Some(jobs.for_engine(1).job())),
            chrono::Utc::now(),
            None,
        );
        ctx.record(
            "claude-code",
            "claude-code:sonnet",
            &round(None),
            chrono::Utc::now(),
            None,
        );
        assert_eq!(cost_rows(&service).len(), 3);
    }

    #[test]
    fn billing_follows_each_engines_metering() {
        let engines = Engines::default();
        for kind in [
            CodingEngineKind::ClaudeCode,
            CodingEngineKind::ClaudeCodeBedrock,
            CodingEngineKind::Codex,
            CodingEngineKind::CodexApi,
            CodingEngineKind::Model,
        ] {
            let engine = engines.get(kind);
            let expected = if engine.metered() {
                Billing::Billed
            } else {
                Billing::Estimated
            };
            assert_eq!(billing_of(engine.label()), expected, "{kind:?}");
        }
        assert_eq!(billing_of(crate::claude_pilot::ENGINE), Billing::Estimated);
        for billed in ["agent", "wasm", "report", "something-new", ""] {
            assert_eq!(billing_of(billed), Billing::Billed, "{billed}");
        }
        assert_eq!(
            serde_json::to_value([Billing::Billed, Billing::Estimated]).unwrap(),
            serde_json::json!(["billed", "estimated"])
        );
    }
}
