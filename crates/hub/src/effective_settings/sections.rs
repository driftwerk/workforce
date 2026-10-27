//! The sections of a settings report, each read from what the runtime
//! reads for it.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};
use workforce_agent::attachments::{Describer, Transcriber, VisionStrategy};
use workforce_agent::tools::loading::ToolLoading;
use workforce_common::rich::RichFeatures;
use workforce_common::session::{CompactionConfig, Session, SpillConfig};
use workforce_common::settings::{
    AdvisorConfig, AgentConfig, AgentDecideFirstUse, AgentWorkspaceConfig, ApprovalsConfig,
    AttachmentsConfig, AwsS3Config, BudgetConfig, ClaimGroundingConfig, CodeContextConfig,
    CompileGateConfig, DecideFirstUse, DecisionConfig, DecisionRecordsConfig, DecisionUse,
    EffectiveBudget, EmbeddingsConfig, HandoffConfig, HandoffPolicy, KnowHowConfig, KnowHowInject,
    LlmConfig, MemoryConfig, ProgramsConfig, ProgramsMode, RefactorConfig, ResearchConfig,
    ResearchPolicy, SchedulerConfig, SelfLearningConfig, SessionsConfig, TelemetryConfig,
    ToolLoadingMode, ToolsConfig, VaultSource, WorkerReadSourcesConfig, WorkforceConfig,
};
use workforce_skills::adapters::slack::BotIdentity;

use super::layers::{within, Layered, SET};
use super::{Row, Source, Status, Viewer};
use crate::credentials::{Caller, LastResort};
use crate::decide::KeySource;
use crate::resolved_config::ResolvedConfig;
use crate::serve::AgentService;

type Section = fn(&Ctx<'_>) -> Vec<Row>;

/// Every section a report can hold, in the order it lists them. One with
/// no function of its own is its resolved table and nothing more.
const SECTIONS: &[(&str, Option<Section>)] = &[
    ("config", Some(config)),
    ("llm", Some(llm)),
    ("memory", None),
    ("compaction", None),
    ("advisor", None),
    ("know_how", Some(know_how)),
    ("tools", Some(tools)),
    ("research", Some(research)),
    ("handoff", Some(handoff)),
    ("code_context", None),
    ("claim_grounding", None),
    ("self_learning", None),
    ("session_digest", None),
    ("worker_read_sources", None),
    ("attachments", Some(attachments)),
    ("decision", Some(decision)),
    ("scheduler", None),
    ("rich", None),
    ("evals", Some(evals)),
    ("budget", Some(budget)),
    ("credentials", Some(credentials)),
    ("slack", Some(slack)),
    ("embeddings", None),
    ("spill", None),
    ("aws_s3", None),
    ("telemetry", None),
    ("approvals", None),
];

pub(super) fn names() -> impl Iterator<Item = &'static str> {
    SECTIONS.iter().map(|(name, _)| *name)
}

pub(super) async fn rows(service: &AgentService, viewer: &Viewer, session: &Session) -> Vec<Row> {
    let resolved = service.resolved_snapshot();
    let ctx = Ctx {
        service,
        install: &service.config,
        agent: service.admission.agent_config(),
        tables: tables(service, &resolved),
        caller: Caller::for_service(service, viewer.person.clone()),
        session,
        slack: service.slack_bot_identities.read().await.clone(),
        resolved,
    };
    SECTIONS
        .iter()
        .flat_map(|(name, section)| match section {
            Some(section) => section(&ctx),
            None => ctx.resolved_table(name),
        })
        .collect()
}

struct Ctx<'a> {
    service: &'a AgentService,
    install: &'a WorkforceConfig,
    /// The agent's `[[agents]]` entry in the install config.
    agent: Option<&'a AgentConfig>,
    resolved: Arc<ResolvedConfig>,
    /// One [`Layered`] per table the resolved config carries.
    tables: Vec<Layered>,
    caller: Caller,
    session: &'a Session,
    slack: HashMap<String, BotIdentity>,
}

impl Ctx<'_> {
    fn resolved_table(&self, section: &str) -> Vec<Row> {
        self.tables
            .iter()
            .filter(|table| table.section() == section)
            .flat_map(Layered::rows)
            .collect()
    }

    fn merged_llm(&self) -> LlmConfig {
        crate::serve::merged_effective_llm(
            self.agent,
            self.resolved.workspace.as_ref(),
            &self.install.llm,
        )
    }
}

/// The tables whose values the agent's resolved config holds, layer by
/// layer. The resolved config, the agent's `config.toml` and the install's
/// `[tools]` and `[attachments]` are each taken apart in full, so a field
/// added to one of them does not compile until it is given a table here.
fn tables(service: &AgentService, resolved: &ResolvedConfig) -> Vec<Layered> {
    let install = &*service.config;
    let block = service.admission.agent_config();
    let ResolvedConfig {
        memory,
        compaction,
        spill,
        advisor,
        routing,
        model_preferences,
        voice_model,
        voice_thinking_budget_tokens,
        aws_s3,
        worker_read_sources_enabled,
        self_learning,
        claim_grounding,
        code_context,
        session_digest,
        programs,
        compile_gate,
        refactor,
        know_how,
        decision,
        rich,
        scheduler,
        research,
        handoff,
        video_frames,
        logins,
        llm_timeouts: _,
        tiers,
        // Shown and edited on the agent's own Environments tab.
        environments: _,
        workspace,
    } = resolved;
    let AgentWorkspaceConfig {
        llm: own_llm,
        compaction: own_compaction,
        memory: own_memory,
        advisor: own_advisor,
        // Who may reach the agent and what it answers them: other people's
        // names, shown on the access pages.
        access: _,
        scope: _,
        // The agent's own clock and report themes, over no install table.
        temporal: _,
        report: _,
        worker_read_sources: own_worker_read_sources,
        self_learning: own_self_learning,
        claim_grounding: own_claim_grounding,
        code_context: own_code_context,
        session_digest: own_session_digest,
        rich: own_rich,
        // Each read from the resolved config above.
        tools: _,
        know_how: _,
        decision: _,
        research: _,
        handoff: _,
        attachments: _,
        scheduler: _,
        // Their own sections, read from the agent's file alone.
        evals: _,
        environments: _,
    } = workspace.clone().unwrap_or_default();
    let ToolsConfig {
        validation,
        loading,
        programs: install_programs,
        compile_gate: install_compile_gate,
        refactor: install_refactor,
    } = &install.tools;
    let AttachmentsConfig {
        video: install_video,
    } = &install.attachments;
    let tools_default = ToolsConfig::default();
    let tier_key = service.tier_key.read().clone();
    let reloaded = crate::serve::merged_effective_llm(block, workspace.as_ref(), &install.llm);
    // As startup would resolve it, so that only a real edit differs from
    // what the agent was built with.
    let reloaded = LlmConfig {
        failover: crate::serve::resolved_failover(&reloaded.failover, tiers, &tier_key),
        ..reloaded
    };
    // The provider the agent is on, in the config's words where the config
    // names it: a swap that failed, or one the config no longer asks for,
    // leaves the two apart.
    let serving = (*service.provider_kind.read(), tier_key.clone());
    let named = |spec: &str| {
        workforce_agent::llm::tiers::parse_provider_spec(spec)
            .ok()
            .flatten()
    };
    let started = &service.effective_llm.provider;
    let provider = match (named(&reloaded.provider), named(started)) {
        (Some(now), _) if now == serving => reloaded.provider.as_str(),
        (_, Some(started)) if started != serving => serving.0.canonical_name(),
        _ => started.as_str(),
    };
    let waiting = service.resolved_at_start.structural_diff(resolved);
    let held = |section: &'static str| {
        waiting
            .iter()
            .filter_map(move |field| field.strip_prefix(section)?.strip_prefix('.'))
    };
    let handoff_policy = |policy: HandoffPolicy| {
        let HandoffPolicy {
            verify_allowed,
            verify_default,
            max_corrections,
            model,
        } = policy;
        json!({
            "allow_verify": verify_allowed,
            "verify": verify_default,
            "max_corrections": max_corrections,
            "model": model,
        })
    };
    let policy = |policy: ResearchPolicy| {
        let ResearchPolicy {
            adversarial_allowed,
            adversarial_default,
            low_confidence_below,
        } = policy;
        json!({
            "allow_adversarial": adversarial_allowed,
            "adversarial": adversarial_default,
            "low_confidence_below": low_confidence_below,
        })
    };

    vec![
        Layered::new("llm", "", &LlmConfig::default(), &install.llm)
            .layer(Source::InstallAgent, block.and_then(|a| a.llm.as_ref()))
            .layer(Source::Agent, own_llm.as_ref())
            .without("routing")
            .admin_only(&["accounts"])
            .effective(&service.effective_llm)
            .effective_field("provider", &provider)
            .rebuilt_on_restart(&reloaded)
            // What a reload applies to the running agent without a restart.
            .effective_field("default_model", &*service.current_default_model.read())
            .effective_field("logins", logins)
            .when(
                service.failover_provider.is_some() || service.token_pool.is_some(),
                |llm| llm.effective_field("retry", &reloaded.retry),
            )
            .when(service.token_pool.is_some(), |llm| {
                llm.effective_field("pacing", &reloaded.pacing)
            })
            .effective_field("image", &*service.image_config.read())
            .effective_field("video", &*service.video_config.read())
            .effective_field("speech", &*service.speech_config.read())
            .effective_field("voice_model", voice_model)
            .effective_field("voice_thinking_budget_tokens", voice_thinking_budget_tokens)
            .effective_field("model_preferences", model_preferences)
            .runtime(&json!({
                "default_model": *service.active_model.read(),
                "max_tokens": *service.active_max_tokens.read(),
                "max_rounds": *service.active_max_rounds.read(),
            })),
        Layered::new("llm", "routing", &routing_default(), &routing_default())
            .layer(
                Source::Agent,
                own_llm.as_ref().and_then(|llm| llm.routing.as_ref()),
            )
            .effective(routing)
            .runtime(&*service.active_routing.read()),
        Layered::new("memory", "", &MemoryConfig::default(), &install.memory)
            .layer(Source::InstallAgent, block.and_then(|a| a.memory.as_ref()))
            .layer(Source::Agent, own_memory.as_ref())
            .effective(memory)
            .held_from_start(&service.resolved_at_start.memory, held("memory")),
        Layered::new(
            "compaction",
            "",
            &CompactionConfig::default(),
            &install.compaction,
        )
        .layer(
            Source::InstallAgent,
            block.and_then(|a| a.compaction.as_ref()),
        )
        .layer(Source::Agent, own_compaction.as_ref())
        .effective(compaction)
        .runtime(&*service.active_compaction.read()),
        Layered::new("advisor", "", &AdvisorConfig::default(), &install.advisor)
            .layer(Source::InstallAgent, block.and_then(|a| a.advisor.as_ref()))
            .layer(Source::Agent, own_advisor.as_ref())
            .admin_only(&["agent_id"])
            .effective(advisor),
        Layered::new("know_how", "", &KnowHowConfig::default(), &install.know_how)
            .layer(Source::Agent, know_how.as_ref())
            .switched(service.know_how(), "mode", &install.know_how.mode),
        Layered::new(
            "tools",
            "programs",
            &ProgramsConfig::default(),
            install_programs,
        )
        .layer(Source::Agent, programs.as_ref())
        .admin_only(&["agents", "except"])
        .switched(service.programs_config(), "mode", &install_programs.mode),
        Layered::new(
            "tools",
            "compile_gate",
            &CompileGateConfig::default(),
            install_compile_gate,
        )
        .layer(Source::Agent, compile_gate.as_ref())
        .admin_only(&["checks"])
        .switched(
            service.compile_gate_config(),
            "mode",
            &install_compile_gate.mode,
        ),
        Layered::new(
            "tools",
            "refactor",
            &RefactorConfig::default(),
            install_refactor,
        )
        .layer(Source::Agent, refactor.as_ref())
        .admin_only(&["rust_analyzer", "typescript"])
        .switched(service.refactor(), "mode", &install_refactor.mode),
        Layered::new("tools", "loading", &tools_default.loading, loading)
            .effective(loading)
            .install_wide(),
        Layered::new("tools", "validation", &tools_default.validation, validation)
            .effective(validation)
            .install_wide(),
        Layered::new(
            "decision",
            "records",
            &DecisionRecordsConfig::default(),
            &install.decision.records,
        )
        .layer(Source::Agent, decision.and_then(|own| own.records).as_ref())
        .effective(&service.decision_records()),
        Layered::new(
            "decision",
            "uses.decide_first",
            &AgentDecideFirstUse {
                mode: Some(DecideFirstUse::default().mode),
            },
            &AgentDecideFirstUse {
                mode: Some(install.decision.uses.decide_first.mode),
            },
        )
        .layer(
            Source::Agent,
            decision
                .and_then(|own| own.uses)
                .and_then(|uses| uses.decide_first)
                .as_ref(),
        )
        .effective(&AgentDecideFirstUse {
            mode: Some(
                install
                    .decision
                    .uses
                    .decide_first
                    .for_agent(decision.as_ref()),
            ),
        }),
        Layered::new(
            "scheduler",
            "",
            &SchedulerConfig::default(),
            &install.scheduler,
        )
        .layer(Source::Agent, scheduler.as_ref())
        .effective(&service.scheduler_policy()),
        Layered::new(
            "rich",
            "",
            &RichFeatures::default(),
            &RichFeatures::default(),
        )
        .layer(Source::Agent, own_rich.as_ref())
        .effective(rich),
        Layered::new(
            "research",
            "",
            &policy(ResearchConfig::default().for_agent(None)),
            &policy(install.research.for_agent(None)),
        )
        .layer(Source::Agent, research.as_ref())
        .effective(&policy(service.research())),
        Layered::new(
            "handoff",
            "",
            &handoff_policy(HandoffConfig::default().for_agent(None)),
            &handoff_policy(install.handoff.for_agent(None)),
        )
        .layer(Source::Agent, handoff.as_ref())
        .effective(&handoff_policy(service.handoff())),
        Layered::new(
            "code_context",
            "",
            &CodeContextConfig::default(),
            &install.code_context,
        )
        .layer(Source::Agent, own_code_context.as_ref())
        .effective(code_context),
        Layered::new(
            "claim_grounding",
            "",
            &ClaimGroundingConfig::default(),
            &install.claim_grounding,
        )
        .layer(Source::Agent, own_claim_grounding.as_ref())
        .effective(claim_grounding),
        Layered::new(
            "self_learning",
            "",
            &SelfLearningConfig::default(),
            &install.self_learning,
        )
        .layer(Source::Agent, own_self_learning.as_ref())
        .effective(self_learning),
        Layered::new(
            "session_digest",
            "",
            &SessionsConfig::default().digest_settings(),
            &install.sessions.digest_settings(),
        )
        .layer(Source::Agent, own_session_digest.as_ref())
        .effective(session_digest),
        Layered::new(
            "worker_read_sources",
            "",
            &WorkerReadSourcesConfig::default(),
            &install.worker_read_sources,
        )
        .layer(Source::Agent, own_worker_read_sources.as_ref())
        .effective(&install.worker_read_sources)
        .effective_field("enabled", worker_read_sources_enabled),
        Layered::new(
            "attachments",
            "video",
            &AttachmentsConfig::default().video,
            install_video,
        )
        .layer(Source::Agent, video_frames.as_ref())
        .switched(service.video_frames(), "enabled", &false),
        Layered::new(
            "embeddings",
            "",
            &EmbeddingsConfig::default(),
            &install.embeddings,
        )
        .effective(&install.embeddings)
        .install_wide(),
        Layered::new("spill", "", &SpillConfig::default(), &install.spill)
            .effective(spill)
            .install_wide(),
        Layered::new("aws_s3", "", &AwsS3Config::default(), &install.aws_s3)
            .effective(aws_s3)
            .held_from_start(&service.resolved_at_start.aws_s3, held("aws_s3"))
            .install_wide(),
        Layered::new(
            "telemetry",
            "",
            &TelemetryConfig::default(),
            &install.telemetry,
        )
        .effective(&install.telemetry)
        .install_wide(),
        Layered::new(
            "approvals",
            "",
            &ApprovalsConfig::default(),
            &install.approvals,
        )
        .effective(&install.approvals)
        .install_wide(),
    ]
}

fn routing_default() -> workforce_agent::model_router::RoutingConfig {
    Default::default()
}

impl Layered {
    /// A table an agent has or has not: `merged` is what the runtime's merge
    /// gave it. With none, only the switch `key` is in effect, as `off`.
    fn switched(self, merged: Option<impl Serialize>, key: &str, off: &impl Serialize) -> Self {
        match merged {
            Some(merged) => self.effective(&merged),
            None => self.effective_field(key, off),
        }
    }

    fn when(self, applies: bool, then: impl FnOnce(Self) -> Self) -> Self {
        match applies {
            true => then(self),
            false => self,
        }
    }
}

/// What decides whether an agent has a switched table: the install's mode,
/// what the agent's own config asks for, and what the merge made of them.
struct Switch {
    on: bool,
    mode: ProgramsMode,
    wanted: Option<bool>,
}

impl Switch {
    /// Why the table is off for this agent, from the same two inputs its
    /// merge read. The merge has already decided that it is.
    fn why_off(&self, what: &str) -> (Status, String) {
        match (self.mode, self.wanted) {
            (ProgramsMode::Off, Some(true)) => (
                Status::Inert,
                format!(
                    "the agent's config.toml enables {what}, but the install has it switched off"
                ),
            ),
            (ProgramsMode::Off, _) => (
                Status::KillSwitched,
                format!("the install has {what} switched off"),
            ),
            (ProgramsMode::OptIn, _) => (
                Status::Off,
                format!("{what} is opt-in and the agent's config.toml does not enable it"),
            ),
            (ProgramsMode::On, Some(false)) => (
                Status::Off,
                format!("the agent's config.toml turns {what} off"),
            ),
            (ProgramsMode::On, _) => (
                Status::Off,
                format!("the install's agent lists leave this agent out of {what}"),
            ),
        }
    }

    /// `rows` (one switched table's) with its state said first and, while it
    /// is off, what the agent set for it marked as not in effect. `shadow`
    /// is why a table that is on changes nothing.
    fn rows(&self, rows: Vec<Row>, table: &str, what: &str, shadow: Option<&str>) -> Vec<Row> {
        let Some(section) = rows.first().map(|row| row.section) else {
            return rows;
        };
        let key = match table.is_empty() {
            true => "state".to_string(),
            false => format!("{table}.state"),
        };
        let state = match (self.on, shadow) {
            (false, _) => {
                let (status, why) = self.why_off(what);
                Row::live(section, key, "off").status(status).because(why)
            }
            (true, Some(why)) => Row::live(section, key, "shadow")
                .status(Status::Shadow)
                .because(why),
            (true, None) => Row::live(section, key, "on").status(Status::Active),
        };
        let unused = state.reason.clone().filter(|_| !self.on);
        std::iter::once(state)
            .chain(rows.into_iter().map(|row| {
                match &unused {
                    Some(why)
                        if row.source == Some(Source::Agent)
                            && row.status.is_none()
                            && !row.key.ends_with("enabled") =>
                    {
                        row.status(Status::Inert)
                            .because(format!("not in effect: {why}"))
                    }
                    _ => row,
                }
            }))
            .collect()
    }
}

fn llm(ctx: &Ctx) -> Vec<Row> {
    let service = ctx.service;
    let (model, how) = service.resolve_model_with_source(ctx.session, None, None);
    let chosen_live = matches!(
        how,
        "playbook step" | "session override" | "agent override" | "agent preference"
    );
    let picked = Row::live("llm", "model", model.clone()).because(format!(
        "this turn's model: {how}, served by {}",
        service.provider_kind.read().canonical_name()
    ));
    let merged = ctx.merged_llm();
    let strategies = service.resolve_media_strategies(&model);
    let strategy = match &strategies.vision {
        VisionStrategy::Native => Row::live("llm", "vision.strategy", "native")
            .because(format!("{model} reads images and documents itself")),
        VisionStrategy::NativeImages => Row::live("llm", "vision.strategy", "native_images")
            .because(format!(
                "{model} reads images itself; documents reach it as text"
            )),
        VisionStrategy::DescribeWith(describer, _) => {
            Row::live("llm", "vision.strategy", "describe_with").because(format!(
                "{model} does not read images; {} describes them",
                describer.model_name()
            ))
        }
        VisionStrategy::StripWithPlaceholder(why) => {
            Row::live("llm", "vision.strategy", "placeholder")
                .status(Status::Inert)
                .because(format!("{model} does not read images and {why}"))
        }
    };
    let (describer, described) = match &service.describer {
        Describer::Ready(_, config) => (
            Built::Ready(
                crate::vision::route(config, &service.vision_tiers)
                    .map(|(kind, model)| format!("{}:{model}", kind.canonical_name())),
            ),
            same(
                Some(config),
                crate::serve::describer_config(&merged).as_ref(),
            ),
        ),
        Describer::Unconfigured => (
            Built::Unconfigured,
            crate::serve::describer_config(&merged).is_none(),
        ),
        Describer::Misconfigured(why) => (Built::Misconfigured(why), true),
    };
    let (transcriber, transcribed) = match &service.transcriber {
        Transcriber::Ready(_, config) => (
            Built::Ready(
                crate::transcription::route(config, &service.transcription_tiers)
                    .map(|(_, provider, model)| format!("{provider}:{model}")),
            ),
            same(
                Some(config),
                crate::serve::transcriber_config(&merged).as_ref(),
            ),
        ),
        Transcriber::Unconfigured => (
            Built::Unconfigured,
            crate::serve::transcriber_config(&merged).is_none(),
        ),
        Transcriber::Misconfigured(why) => (Built::Misconfigured(why), true),
    };
    ctx.resolved_table("llm")
        .into_iter()
        .chain([match chosen_live {
            true => picked.from(Source::Runtime),
            false => picked,
        }])
        .chain(describer.rows("vision", described))
        .chain([strategy])
        .chain(transcriber.rows("transcription", transcribed))
        .collect()
}

fn same<T: Serialize>(built: Option<&T>, configured: Option<&T>) -> bool {
    serde_json::to_value(built).ok() == serde_json::to_value(configured).ok()
}

/// What an agent's startup left of a secondary model (`[llm.vision]`,
/// `[llm.transcription]`): the provider and model it built, nothing, or why
/// it could build nothing.
enum Built<'a> {
    Ready(eyre::Result<String>),
    Unconfigured,
    Misconfigured(&'a str),
}

impl Built<'_> {
    /// `current` is whether the config as it stands now is the one this was
    /// built from; only a restart rebuilds it.
    fn rows(self, table: &str, current: bool) -> Vec<Row> {
        let state = |value: &str| Row::live("llm", format!("{table}.state"), value);
        let stale = (!current).then(|| {
            Row::live("llm", format!("{table}.pending"), true)
                .status(Status::Inert)
                .because(format!(
                    "[llm.{table}] changed since the agent started; the change applies after a \
                     restart"
                ))
        });
        let rows = match self {
            Self::Ready(route) => vec![
                state("ready").status(Status::Active),
                match route {
                    Ok(route) => Row::live("llm", format!("{table}.built"), route),
                    Err(e) => Row::live("llm", format!("{table}.built"), Value::Null)
                        .because(e.to_string()),
                },
            ],
            Self::Unconfigured => vec![state("unconfigured").status(Status::Off).because(format!(
                "no [llm.{table}] is configured, or it is switched off"
            ))],
            Self::Misconfigured(why) => vec![state("misconfigured")
                .status(Status::Inert)
                .because(format!("{table} is misconfigured: {why}"))],
        };
        rows.into_iter().chain(stale).collect()
    }
}

fn know_how(ctx: &Ctx) -> Vec<Row> {
    let merged = ctx.service.know_how();
    let shadow = merged
        .as_ref()
        .is_some_and(|config| config.inject == KnowHowInject::Shadow)
        .then_some("matching know-how is recorded, not shown to the model (inject = \"shadow\")");
    let copy = ctx
        .resolved
        .workspace
        .as_ref()
        .is_some_and(AgentWorkspaceConfig::is_test_copy);
    let rows = ctx
        .resolved_table("know_how")
        .into_iter()
        .map(|row| match row.key == "source_agent" && !copy {
            true => row.status(Status::Inert).because(
                "only a test copy ([evals] copy = true) reads another agent's know-how; this \
                 agent reads its own",
            ),
            false => row,
        })
        .collect();
    Switch {
        on: merged.is_some(),
        mode: ctx.install.know_how.mode,
        wanted: ctx.resolved.know_how.as_ref().and_then(|own| own.enabled),
    }
    .rows(rows, "", "know-how", shadow)
}

fn tools(ctx: &Ctx) -> Vec<Row> {
    let (service, install, own) = (ctx.service, &ctx.install.tools, &ctx.resolved);
    let rows = ctx.resolved_table("tools");
    let table = |table: &str, what: &str, switch: Switch| {
        switch.rows(
            rows.iter()
                .filter(|row| within(&row.key, table))
                .cloned()
                .collect(),
            table,
            what,
            None,
        )
    };
    let programs = table(
        "programs",
        "programs",
        Switch {
            on: service.programs_config().is_some(),
            mode: install.programs.mode,
            wanted: own.programs.as_ref().and_then(|own| own.enabled),
        },
    );
    let compile_gate = table(
        "compile_gate",
        "the compile gate",
        Switch {
            on: service.compile_gate_config().is_some(),
            mode: install.compile_gate.mode,
            wanted: own.compile_gate.as_ref().and_then(|own| own.enabled),
        },
    );
    let refactor = table(
        "refactor",
        "code.refactor",
        Switch {
            on: service.refactor().is_some(),
            mode: install.refactor.mode,
            wanted: own.refactor.as_ref().and_then(|own| own.enabled),
        },
    );
    let ToolLoading { mode, core, always } = ctx.service.tool_loading();
    let loading = match (mode, install.loading.mode) {
        (ToolLoadingMode::On, _) => Row::live("tools", "loading.state", "on")
            .status(Status::Active)
            .because("tools outside the resident and in-use sets are loaded on demand"),
        (ToolLoadingMode::Shadow, _) => Row::live("tools", "loading.state", "shadow")
            .status(Status::Shadow)
            .because("the loading plan is worked out and logged; every tool is still offered"),
        (ToolLoadingMode::Off, ToolLoadingMode::Off) => Row::live("tools", "loading.state", "off")
            .status(Status::KillSwitched)
            .because("the install has tool loading switched off"),
        (ToolLoadingMode::Off, _) => Row::live("tools", "loading.state", "off")
            .status(Status::Off)
            .because("the install's agent lists leave this agent out of tool loading"),
    };
    programs
        .into_iter()
        .chain(compile_gate)
        .chain(refactor)
        .chain([
            loading,
            Row::live("tools", "loading.resident", json!(*always))
                .because("always offered in full"),
            match core {
                Some(core) => Row::live("tools", "loading.in_use", json!(*core))
                    .because("offered in full because the agent has been calling them"),
                None => Row::live("tools", "loading.in_use", Value::Null)
                    .because("the agent's tool usage has not been read yet"),
            },
        ])
        .chain(
            rows.iter()
                .filter(|row| {
                    ["loading", "validation"]
                        .iter()
                        .any(|t| within(&row.key, t))
                })
                .cloned(),
        )
        .collect()
}

fn research(ctx: &Ctx) -> Vec<Row> {
    let policy = ctx.service.research();
    ctx.resolved_table("research")
        .into_iter()
        .map(|row| match row.key.as_str() {
            "adversarial" if policy.adversarial_default && !policy.adversarial_allowed => {
                row.status(Status::Inert).because(
                    "the agent's config.toml asks for adversarial research, but the install \
                     forbids it (allow_adversarial = false)",
                )
            }
            "allow_adversarial" if !policy.adversarial_allowed => row
                .status(Status::KillSwitched)
                .because("the install forbids adversarial research for every agent"),
            _ => row,
        })
        .collect()
}

fn handoff(ctx: &Ctx) -> Vec<Row> {
    let policy = ctx.service.handoff();
    ctx.resolved_table("handoff")
        .into_iter()
        .map(|row| match row.key.as_str() {
            "verify" if policy.verify_default && !policy.verify_allowed => {
                row.status(Status::Inert).because(
                    "the agent's config.toml asks for hand-overs to be verified, but the install \
                     forbids it (allow_verify = false)",
                )
            }
            "allow_verify" if !policy.verify_allowed => row
                .status(Status::KillSwitched)
                .because("the install forbids verifying hand-overs for every agent"),
            _ => row,
        })
        .collect()
}

/// Whether the agent's `config.toml` as it stands is the one in effect.
fn config(ctx: &Ctx) -> Vec<Row> {
    let service = ctx.service;
    let state = |value: &str| Row::live("config", "state", value);
    let file = service.workspace.join("config.toml").display().to_string();
    vec![
        match (&*service.config_rejected.read(), &ctx.resolved.workspace) {
            // Only the error's first line: a parse error goes on to quote the
            // line it failed on, which may hold a key.
            (Some(why), _) => state("rejected").status(Status::Inert).because(format!(
                "the agent's config.toml was refused when it last changed, so every value here \
                 is from the last one accepted: {}",
                why.lines()
                    .next()
                    .unwrap_or_default()
                    .replace(&file, "config.toml")
            )),
            (None, Some(_)) => state("applied").status(Status::Active),
            (None, None) => state("none")
                .status(Status::Off)
                .because("the agent has no config.toml; the install config and the defaults apply"),
        },
    ]
}

/// Whether the agent is a test copy `workforce eval` may lay overrides over.
fn evals(ctx: &Ctx) -> Vec<Row> {
    let copy = ctx
        .resolved
        .workspace
        .as_ref()
        .and_then(|own| own.evals)
        .is_some_and(|evals| evals.copy);
    vec![Row::live("evals", "copy", copy).status(match copy {
        true => Status::Active,
        false => Status::Off,
    })]
}

fn attachments(ctx: &Ctx) -> Vec<Row> {
    Switch {
        on: ctx.service.video_frames().is_some(),
        mode: match ctx.install.attachments.video.enabled {
            true => ProgramsMode::On,
            false => ProgramsMode::Off,
        },
        wanted: ctx
            .resolved
            .video_frames
            .as_ref()
            .and_then(|own| own.enabled),
    }
    .rows(
        ctx.resolved_table("attachments"),
        "video",
        "video frames",
        None,
    )
}

fn level(tier: VaultSource) -> &'static str {
    match tier {
        VaultSource::AgentPerUser => "your own key for this agent",
        VaultSource::GlobalPerUser => "your own key",
        VaultSource::AgentSpecific => "agent level",
        VaultSource::Global => "global level",
        VaultSource::Unknown => "vault",
    }
}

fn decision(ctx: &Ctx) -> Vec<Row> {
    // `[decision.records]` and `[decision.uses.decide_first] mode` are
    // agent-layered tables of their own.
    let install_wide = |config: &DecisionConfig| {
        let mut table = json!(config);
        if let Some(table) = table.as_object_mut() {
            table.remove("records");
        }
        if let Some(first) = table.pointer_mut("/uses/decide_first") {
            first.as_object_mut().map(|first| first.remove("mode"));
        }
        table
    };
    let allowed = ctx.install.decision.uses.decide_first.mode;
    let install = install_wide(&ctx.install.decision);
    decision_rows(crate::decide::decisions().as_deref(), &ctx.caller)
        .into_iter()
        .chain(
            Layered::new(
                "decision",
                "",
                &install_wide(&DecisionConfig::default()),
                &install,
            )
            .effective(&install)
            .install_wide()
            .rows(),
        )
        .chain(ctx.resolved_table("decision").into_iter().map(|row| {
            let enabled = row.value == json!(true);
            match row.key.as_str() {
                "records.enabled" if !ctx.install.decision.records.enabled => row
                    .status(Status::KillSwitched)
                    .because("the install records no decisions for any agent"),
                // Switched on is not the same as kept: say so when decisions
                // are made here and none is being recorded.
                "records.enabled"
                    if enabled
                        && ctx.install.decision.enabled
                        && crate::decision_records::recorder().is_none() =>
                {
                    row.status(Status::Inert).because(
                        "nothing is recorded: the install has no Postgres, or the record \
                         store could not be opened at boot",
                    )
                }
                "uses.decide_first.mode" => {
                    decide_first_row(row, ctx.install.decision.enabled.then_some(allowed))
                }
                // How long the install keeps records is the install's alone.
                "records.retention_days" => Row {
                    install_wide: true,
                    ..row
                },
                _ => row,
            }
        }))
        .collect()
}

/// The agent's `decide_first` rollout, `allowed` being how far the install
/// takes it (`None` with `[decision]` off). A row at its built-in default
/// that no layer tried to move says nothing more.
fn decide_first_row(row: Row, allowed: Option<DecisionUse>) -> Row {
    if row.source == Some(Source::Default) && row.status.is_none() {
        return row;
    }
    match allowed {
        None | Some(DecisionUse::Off) => row
            .status(Status::KillSwitched)
            .because("the install asks no workflow step's outcome before its turn"),
        Some(_) if row.value == json!(DecisionUse::Off) => row.status(Status::Off),
        Some(_) if row.value == json!(DecisionUse::Shadow) => row
            .status(Status::Shadow)
            .because("asked and recorded beside the turn, which always runs"),
        Some(_) => row
            .status(Status::Active)
            .because("a step whose own decide_first allows it is settled when the model is sure"),
    }
}

/// The install's decision setup as `caller` can use it.
fn decision_rows(decisions: Option<&crate::decide::Decisions>, caller: &Caller) -> Vec<Row> {
    let Some(decisions) = decisions else {
        return vec![Row::live("decision", "state", "off")
            .status(Status::Off)
            .because("the install has no decision backends enabled")];
    };
    let backends = decisions.access(caller);
    let usable = backends.iter().any(|backend| backend.available());
    let state = match usable {
        true => Row::live("decision", "state", "on").status(Status::Active),
        false => Row::live("decision", "state", "unavailable")
            .status(Status::Inert)
            .because("decision backends are configured, but none has a key you can use"),
    };
    let rollout = |key: &str, mode: DecisionUse| {
        let row = Row::live("decision", key, json!(mode));
        match mode {
            DecisionUse::Off => row.status(Status::Off),
            DecisionUse::Shadow => row
                .status(Status::Shadow)
                .because("asked alongside the existing path and logged; it decides nothing"),
            DecisionUse::On => row.status(Status::Active),
        }
    };
    let switch = |key: &str, on: bool| {
        Row::live("decision", key, on).status(match on {
            true => Status::Active,
            false => Status::Off,
        })
    };
    std::iter::once(state)
        .chain(backends.iter().map(|backend| {
            let key = format!("backends.{}", backend.name);
            let role = match backend.primary {
                true => "primary",
                false => "fallback",
            };
            match backend.key {
                KeySource::Vault(tier) => Row::live("decision", key, "available")
                    .from(Source::Vault)
                    .status(Status::Active)
                    .because(format!("{role}; key: set ({})", level(tier))),
                KeySource::InstallConfig => Row::live("decision", key, "available")
                    .from(Source::Install)
                    .status(Status::Active)
                    .because(format!("{role}; key: set in the install config")),
                KeySource::Missing if !backend.requires_key => {
                    Row::live("decision", key, "available")
                        .status(Status::Active)
                        .because(format!("{role}; needs no key"))
                }
                KeySource::Missing => Row::live("decision", key, "unavailable")
                    .status(Status::Inert)
                    .because(format!(
                        "{role}; key: not set at any level. {}",
                        crate::credentials::how_to_add(&backend.path)
                    )),
            }
        }))
        .chain([
            switch("uses.tool", decisions.tool_enabled()),
            rollout("uses.scope_gate", decisions.scope_gate().mode),
            rollout("uses.routing", decisions.routing().mode),
            rollout("uses.decide_first", decisions.decide_first_use().mode),
            match decisions.branch_fallback() {
                None => switch("branch_fallback", false),
                Some(false) => switch("branch_fallback", true),
                Some(true) => Row::live("decision", "branch_fallback", "shadow")
                    .status(Status::Shadow)
                    .because("its pick is recorded; the workflow branch is not changed"),
            },
        ])
        .collect()
}

fn budget(ctx: &Ctx) -> Vec<Row> {
    budget_rows(
        &ctx.service.cost_governor,
        ctx.service.agent_id.as_str(),
        ctx.caller.person(),
    )
}

/// The ceilings `governor` holds `agent_id` and `person` to, with the live
/// limit standing in wherever an override moved it.
fn budget_rows(
    governor: &crate::cost_governor::CostGovernor,
    agent_id: &str,
    person: Option<&str>,
) -> Vec<Row> {
    let config = governor.config();
    let agent_id = agent_id.to_string();
    let ceilings = |config: &BudgetConfig| {
        let EffectiveBudget {
            agent_daily_usd,
            job_total_usd,
            job_max_runs,
            job_expiry_days,
        } = config.for_agent(&agent_id);
        json!({
            "daily_usd": agent_daily_usd,
            "job_total_usd": job_total_usd,
            "job_max_runs": job_max_runs,
            "job_expiry_days": job_expiry_days,
        })
    };
    let live = governor
        .status(std::slice::from_ref(&agent_id), person, false)
        .inspect_err(|e| tracing::warn!(target: "settings", error = %e, "budget status unreadable"))
        .ok();
    let limit = |scope: Option<&crate::cost_governor::ScopeStatus>| json!({ "daily_usd": scope.map(|scope| scope.limit_usd) });
    let state = match config.enabled {
        true => Row::live("budget", "state", "on").status(Status::Active),
        false => Row::live("budget", "state", "off")
            .status(Status::KillSwitched)
            .because("the install has budgets switched off; no ceiling is enforced"),
    };
    let defaults = BudgetConfig::default();
    let mine = person.map(|_| {
        Layered::new(
            "budget",
            "you",
            &json!({ "daily_usd": defaults.identity_daily_usd }),
            &json!({ "daily_usd": config.identity_daily_usd }),
        )
        .effective(&json!({ "daily_usd": config.identity_daily_usd }))
        .runtime(&limit(live.as_ref().and_then(|s| s.identities.first())))
    });
    std::iter::once(state)
        .chain(
            Layered::new("budget", "agent", &ceilings(&defaults), &ceilings(config))
                .effective(&ceilings(config))
                .runtime(&limit(live.as_ref().and_then(|s| s.agents.first())))
                .rows(),
        )
        .chain(mine.iter().flat_map(Layered::rows))
        .chain(
            Layered::new("budget", "", &defaults, config)
                .effective(config)
                .runtime(&json!({ "hub_daily_usd": live.as_ref().map(|s| s.hub.limit_usd) }))
                .install_wide()
                .rows(),
        )
        .map(
            |row| match row.key.ends_with("daily_usd") && row.value == json!(0.0) {
                true => row.because("0 is no ceiling"),
                false => row,
            },
        )
        .collect()
}

fn credentials(ctx: &Ctx) -> Vec<Row> {
    let service = ctx.service;
    // The hub's own clients are built once for every agent and read the
    // global vault alone.
    let hub = Caller::new(
        ctx.caller.person().map(str::to_string),
        None,
        service.global_vault.clone(),
    );
    crate::credentials::readers(service)
        .into_iter()
        .map(|(credential, legacy)| {
            let caller = match credential.hub_wide {
                true => &hub,
                false => &ctx.caller,
            };
            Row {
                install_wide: credential.hub_wide,
                ..credential_row(
                    credential,
                    caller.level(credential.path, credential.scope),
                    legacy.held().map(|(from, _)| from),
                )
            }
        })
        .collect()
}

/// Whether `credential` is set for the caller: `held` is the vault level
/// that has it, `last_resort` what is read when none does.
fn credential_row(
    credential: &crate::credentials::VaultCredential,
    held: Option<VaultSource>,
    last_resort: Option<LastResort>,
) -> Row {
    let label = credential.label;
    let set = |source, used: String| {
        Row::live("credentials", credential.path, SET)
            .from(source)
            .status(Status::Active)
            .because(format!("{label}: no vault level holds one; {used} is used"))
    };
    match (held, last_resort) {
        (Some(tier), _) => Row::live("credentials", credential.path, SET)
            .from(Source::Vault)
            .status(Status::Active)
            .because(format!("{label}: set ({})", level(tier))),
        (None, Some(LastResort::Config(key))) => {
            set(Source::Install, format!("the install config's {key}"))
        }
        (None, Some(LastResort::Environment(var))) => {
            set(Source::Environment, format!("the hub's {var}"))
        }
        (None, Some(LastResort::Account)) => set(
            Source::Vault,
            "the provider's LLM account in the vault".to_string(),
        ),
        (None, Some(LastResort::Chain)) => set(
            Source::Environment,
            "the provider's own credential chain".to_string(),
        ),
        (None, None) => Row::live("credentials", credential.path, "not set")
            .status(Status::Off)
            .because(format!(
                "{label}: not set at any level. {}",
                crate::credentials::how_to_add(credential.path)
            )),
    }
}

fn slack(ctx: &Ctx) -> Vec<Row> {
    let mut workspaces: Vec<_> = ctx.slack.iter().collect();
    workspaces.sort_by_key(|(id, _)| id.as_str());
    let state = match workspaces.is_empty() {
        true => Row::live("slack", "state", "off")
            .status(Status::Off)
            .because("no Slack workspace is connected to this agent"),
        false => Row::live("slack", "state", "on").status(Status::Active),
    };
    std::iter::once(state)
        .chain(workspaces.into_iter().map(|(id, bot)| {
            Row::live(
                "slack",
                format!("workspaces.{id}"),
                bot.display_name.clone(),
            )
            .because(format!("connected as bot user {}", bot.user_id))
        }))
        .collect()
}

#[cfg(test)]
mod tests;
