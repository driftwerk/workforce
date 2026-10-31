//! `coding.fanout` — parallel divide-and-conquer coding.
//!
//! Splits a coding job into independent subtasks and drives one ephemeral
//! Claude Code coder per subtask **in parallel**, each in its own isolated
//! worktree, each opening **its own PR**. Phase 1 does no auto-merge: N
//! independent instructions → N PRs the agent/human reviews. For tightly
//! coupled work (where one subtask needs another's output) use `coding.session`
//! — the subtasks here run blind to each other.
//!
//! Where `coding.session` is one serialized, session-resuming driver, this is N
//! ephemeral coders (`session_id: None`, no resume, no persistence). Isolation
//! comes free from [`work_session`]: distinct `scope_id`s give distinct
//! detached worktrees over the shared base. The base itself is provisioned
//! sequentially (concurrent `ensure` would race it); only the coders run in
//! parallel. Results stream back to the originating channel like `investigate`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use workforce_agent::runner::{ToolInvocation, CURRENT_USER_ID};
use workforce_agent::tools::exec_runner::JobId;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_agent::turn_journal::{PendingCall, ToolCall, TurnJournal};
use workforce_common::channels::{ChannelSurface, OutboundMessage};
use workforce_common::session::{JournalEntry, JournalStep, SessionMessage, SessionStore};
use workforce_common::subscription::SCHEDULER;
use workforce_projects::{Project, ProjectStore, RepoSource};

use crate::agent_registry::AgentRegistry;
use crate::claude_code::{self, RoundJobs, RoundKey};
use crate::coding_engine::{self, CodingEngine, RoundContext};
use crate::coding_sandbox::{HomeSpec, SandboxLauncher, WorkspaceSpec};
use crate::coding_session_tool::{
    driver_append_prompt, parse_sentinel, DriverProfile, InFlightGuard, Workspace,
    CODING_BASE_TOOLS,
};
use crate::investigate::truncate;
use crate::projects::work_session;
use crate::serve::AgentService;
use crate::session_turn_claim::SessionTurnClaim;
use crate::stop_registry::{BackgroundWork, CancelTarget};
use crate::turn_journal::{self, SessionJournal};

/// Scope-id prefix for fan-out worktrees, so a leftover (the detached executor
/// dies on redeploy) is unambiguously identifiable and sweepable without
/// touching `coding.session`'s persisted driver worktrees.
const SCOPE_PREFIX: &str = "fanout-";

/// Upper bound on subtasks per call — caps upfront worktree disk + total cost.
/// Excess subtasks are dropped and the caller is told how many ran.
const MAX_SUBTASKS: usize = 12;

/// Minimum subtasks to fan out; fewer is a single coding round.
const MIN_SUBTASKS: usize = 2;

/// Process-wide set of fan-out scope ids with a live worktree, so the
/// orphan sweep skips a concurrent fan-out's in-flight worktrees. After a
/// restart it starts empty: the scopes of a wave some replica may still take
/// up are known from its journal, and every other on-disk `fanout-*` scope is
/// genuinely orphaned. One set covers every concurrent fan-out across
/// agents/threads in this process.
static ACTIVE_SCOPES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn lock_active() -> MutexGuard<'static, HashSet<String>> {
    ACTIVE_SCOPES
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn fanout_directive(scope_short: &str) -> String {
    format!(
        "You are ONE of several coders running in parallel on independent subtasks of a larger \
         job, each in a separate worktree. Implement ONLY the instruction you are given; do not \
         touch files another subtask is likely to own. You MUST open a PR for your work. The \
         parallel coders share this repo's branch namespace, so to avoid collisions create your \
         branch under the unique prefix `fanout/{scope_short}/` (e.g. \
         `fanout/{scope_short}/add-widget`)."
    )
}

/// `coding.fanout` — parallel coders, one PR each.
pub struct CodingFanoutTool {
    service: Arc<AgentService>,
    project_store: Arc<dyn ProjectStore>,
    profile: DriverProfile,
    launcher: Arc<dyn SandboxLauncher>,
    engine: Arc<dyn CodingEngine>,
    in_flight: Arc<Mutex<HashSet<String>>>,
}

#[derive(Deserialize)]
struct Input {
    project: String,
    #[serde(default)]
    subtasks: Vec<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
}

/// Outcome of one coder, mapped from its sentinels / exit status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
enum CoderOutcome {
    /// A PR was opened (or an existing one found).
    Pr(String),
    /// The coder finished but opened no PR (e.g. `BLOCKED`, or done with no
    /// push) — surface its status rather than mislabel it done-or-failed.
    NoPr(String),
    /// Provision failed, the subprocess errored, or the coder never ran.
    Failed(String),
}

/// What a coder a restart cut short reports when its round cannot be
/// collected.
const OUTCOME_LOST: &str =
    "a hub restart cut this coder short and its outcome is unknown; check its branch before redoing it";

/// The journal calls a wave keeps: its plan, and one per coder.
const WAVE_CALL: &str = "wave";
const WAVE_TOOL: &str = "coding.fanout";
const CODER_TOOL: &str = "coding.fanout.coder";

fn coder_call(index: usize) -> String {
    format!("coder-{index}")
}

fn coder_index(call: &str) -> Option<usize> {
    call.strip_prefix("coder-")?.parse().ok()
}

/// The mark of a journalled coder whose worker does not keep its jobs: a
/// restart loses its round, which is then reported rather than run again.
const UNKEPT: &str = "unkept";

/// One subtask, the scope its workspace lives under, the round it runs and
/// the job that round last reached.
#[derive(Clone, Serialize, Deserialize)]
struct Coder {
    index: usize,
    instruction: String,
    scope: String,
    #[serde(flatten)]
    key: RoundKey,
    #[serde(skip)]
    reached: Option<String>,
}

impl Coder {
    /// The jobs its round runs: on from the one it reached, else from its
    /// first. `None` when the mark is not one of the round's.
    fn jobs(&self) -> Option<RoundJobs> {
        match &self.reached {
            Some(mark) => self.key.resume_at(mark).map(|resume| resume.jobs),
            None => Some(self.key.jobs(false)),
        }
    }

    /// The job it last reached, or would start with, for ending it.
    fn job(&self) -> JobId {
        self.reached
            .clone()
            .map(JobId::new)
            .unwrap_or_else(|| self.key.jobs(false).job())
    }
}

/// A progress card a wave posted: where it went and the message there.
#[derive(Clone, Serialize, Deserialize)]
struct Card {
    platform: String,
    message_id: String,
}

/// What a wave runs and where it reports, as its executor needs it.
#[derive(Clone, Serialize, Deserialize)]
struct WaveSpec {
    session_id: String,
    project: String,
    identity: Option<String>,
    requester: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    max_parallel: usize,
    subtasks: Vec<String>,
    channel: Option<String>,
    thread: Option<String>,
}

/// All a replica needs to take up a wave another left.
#[derive(Serialize, Deserialize)]
struct WavePlan {
    #[serde(flatten)]
    spec: WaveSpec,
    cards: Vec<Card>,
}

/// A detached wave's journal in the session store and its hold on it: the
/// turn claim keeps other replicas off it, and [`ActiveWave`] this one,
/// whose claims are re-entrant.
struct Wave {
    journal: Arc<SessionJournal>,
    _claim: SessionTurnClaim,
    _active: ActiveWave,
}

impl Wave {
    /// Hold `key` for a wave this replica runs, when nobody else does.
    fn hold(sessions: &Arc<dyn SessionStore>, key: &str) -> Result<Option<Self>> {
        let Some(active) = ActiveWave::enter(key) else {
            return Ok(None);
        };
        Ok(
            SessionTurnClaim::try_acquire(sessions, key)?.map(|claim| Self {
                journal: SessionJournal::detached(Arc::clone(sessions), key),
                _claim: claim,
                _active: active,
            }),
        )
    }
}

/// The waves this process is running, so its own sweep leaves them alone.
static ACTIVE_WAVES: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

struct ActiveWave(String);

impl ActiveWave {
    fn enter(key: &str) -> Option<Self> {
        ACTIVE_WAVES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.to_string())
            .then(|| Self(key.to_string()))
    }
}

impl Drop for ActiveWave {
    fn drop(&mut self) {
        ACTIVE_WAVES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// Every agent's fan-out, for the sweep that takes up waves a stopped
/// replica left.
static HOSTS: LazyLock<Mutex<HashMap<String, Weak<CodingFanoutTool>>>> =
    LazyLock::new(Default::default);

/// Take up every wave a stopped replica left for the agents in `registry`.
/// Run by the replica that leads, with the resume sweep.
pub async fn recover_waves(registry: &AgentRegistry) {
    for service in registry.all_services() {
        let host = HOSTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(service.agent_id.as_str())
            .and_then(Weak::upgrade);
        if let Some(fanout) = host {
            fanout.recover().await;
        }
    }
}

/// Run a detached wave, registered with the drain so that it is let go of
/// before this hub's claims are released.
fn spawn_wave(wave: impl std::future::Future<Output = ()> + Send + 'static) {
    let task = tokio::spawn(wave);
    let drain = crate::shutdown::Detached::register(task.abort_handle());
    if crate::shutdown::in_progress() {
        task.abort();
    }
    tokio::spawn(async move {
        let _drain = drain;
        let _ = task.await;
    });
}

/// Trim, drop blanks, and cap subtasks. Returns the kept instructions and how
/// many were dropped by the cap.
fn sanitise_subtasks(raw: Vec<String>) -> (Vec<String>, usize) {
    let mut kept: Vec<String> = raw
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let dropped = kept.len().saturating_sub(MAX_SUBTASKS);
    kept.truncate(MAX_SUBTASKS);
    (kept, dropped)
}

/// Short, stable label for a scope id (`fanout-<uuid>` → first 8 of the uuid).
fn scope_short(scope: &str) -> &str {
    let raw = scope.strip_prefix(SCOPE_PREFIX).unwrap_or(scope);
    &raw[..raw.len().min(8)]
}

/// Map a finished Claude Code result to a [`CoderOutcome`].
fn outcome_from_result(result: &Result<claude_code::ClaudeCodeResult>) -> CoderOutcome {
    match result {
        Ok(r) if r.completion.is_usable() => match parse_sentinel(&r.text, "PR_URL:") {
            Some(url) => CoderOutcome::Pr(url),
            None => CoderOutcome::NoPr(
                parse_sentinel(&r.text, "STATUS:")
                    .unwrap_or_else(|| "finished without a PR".into()),
            ),
        },
        other => CoderOutcome::Failed(
            claude_code::classify_failure(other)
                .map(|f| f.human_reason())
                .unwrap_or_else(|| "Claude Code failed for an unknown reason.".to_string()),
        ),
    }
}

/// The scopes of every coder a wave's journal holds.
fn coder_scopes(entries: &[JournalEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| match &entry.step {
            JournalStep::Pending { tool, plan, .. } if tool == CODER_TOOL => {
                serde_json::from_value::<Coder>(plan.clone()).ok()
            }
            _ => None,
        })
        .map(|coder| coder.scope)
        .collect()
}

/// A coder's outcome as its journalled call returned it.
fn outcome_of(row: &SessionMessage) -> Option<(usize, CoderOutcome)> {
    let index = coder_index(&row.id)?;
    let output = row.tool_data.as_ref()?.get("output")?.clone();
    Some((index, serde_json::from_value(output).ok()?))
}

fn outcome_emoji(outcome: Option<&CoderOutcome>) -> &'static str {
    match outcome {
        None => "🔄",
        Some(CoderOutcome::Pr(_)) => "✅",
        Some(CoderOutcome::NoPr(_)) => "⚠️",
        Some(CoderOutcome::Failed(_)) => "❌",
    }
}

fn outcome_suffix(outcome: Option<&CoderOutcome>) -> String {
    match outcome {
        None => String::new(),
        Some(CoderOutcome::Pr(url)) => format!(" → {url}"),
        Some(CoderOutcome::NoPr(status)) => format!(" — {}", truncate(status, 80)),
        Some(CoderOutcome::Failed(reason)) => format!(" — {}", truncate(reason, 120)),
    }
}

/// Render the progress/summary message from the per-subtask outcomes (a `None`
/// slot is still running). Markdown bullet list so the Slack renderer keeps the
/// lines separate.
fn render_progress(subtasks: &[String], outcomes: &[Option<CoderOutcome>]) -> String {
    let done = outcomes.iter().filter(|o| o.is_some()).count();
    let mut out = format!("🛠️ *Coding fan-out* — {done}/{} subtasks\n", subtasks.len());
    for (instruction, outcome) in subtasks.iter().zip(outcomes) {
        out.push_str(&format!(
            "\n- {} {}{}",
            outcome_emoji(outcome.as_ref()),
            truncate(instruction, 80),
            outcome_suffix(outcome.as_ref())
        ));
    }
    out
}

impl CodingFanoutTool {
    pub fn new(
        service: Arc<AgentService>,
        project_store: Arc<dyn ProjectStore>,
        profile: DriverProfile,
        launcher: Arc<dyn SandboxLauncher>,
        engine: Arc<dyn CodingEngine>,
    ) -> Self {
        Self {
            service,
            project_store,
            profile,
            launcher,
            engine,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Let the resume sweep take up this agent's waves.
    pub fn host(self: &Arc<Self>) {
        HOSTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                self.service.agent_id.as_str().to_string(),
                Arc::downgrade(self),
            );
    }

    /// The executor for `spec` over `project`, with the credentials its repos
    /// are fetched with.
    fn executor(
        &self,
        spec: WaveSpec,
        project: Project,
        wave: Option<Wave>,
        guard: Option<InFlightGuard>,
    ) -> Executor {
        let tokens = work_session::resolve_github_tokens(
            &project,
            self.service.vault.as_ref(),
            self.service.global_vault.as_ref(),
        );
        let gh_token = project.repos.first().and_then(|r| {
            let RepoSource::Github { owner, .. } = &r.source;
            tokens.get(owner).cloned()
        });
        Executor {
            service: Arc::clone(&self.service),
            agent_id: self.service.agent_id.as_str().to_string(),
            profile: self.profile.clone(),
            launcher: Arc::clone(&self.launcher),
            engine: Arc::clone(&self.engine),
            project,
            tokens,
            gh_token,
            spec,
            token: CancellationToken::new(),
            work_id: format!("coding-fanout-{}", Uuid::new_v4()),
            wave,
            _guard: guard,
        }
    }

    /// Take up every wave this agent's journals hold that nobody runs.
    async fn recover(&self) {
        let keys = match self.service.sessions.journals(SCOPE_PREFIX) {
            Ok(keys) => keys,
            Err(e) => {
                tracing::warn!(error = %e, "listing fan-out waves to take up");
                return;
            }
        };
        for key in keys {
            if let Err(e) = self.take_up(&key).await {
                tracing::warn!(wave = %key, error = %e, "a fan-out wave could not be taken up");
            }
        }
    }

    /// Take up the wave journalled under `key`, unless it is running:
    /// collect its coders from where they got to and finish it as its own
    /// executor would have.
    async fn take_up(&self, key: &str) -> Result<()> {
        let sessions = &self.service.sessions;
        let Some(wave) = Wave::hold(sessions, key)? else {
            return Ok(());
        };
        let entries = sessions.journal(key)?;
        let left = turn_journal::left_in(&entries);
        let Some(plan) = left
            .collect
            .iter()
            .chain(&left.lost)
            .find(|call| call.call == WAVE_CALL)
        else {
            return sessions.journal_clear(key);
        };
        let plan: WavePlan = match serde_json::from_value(plan.plan.clone()) {
            Ok(plan) => plan,
            Err(e) => {
                tracing::warn!(wave = %key, error = %e, "a fan-out wave's plan is unreadable; letting it go");
                return sessions.journal_clear(key);
            }
        };
        let size = plan.spec.subtasks.len();
        let coder = |call: &PendingCall| {
            (call.tool == CODER_TOOL)
                .then(|| serde_json::from_value::<Coder>(call.plan.clone()).ok())
                .flatten()
                .filter(|coder| coder.index < size)
                .map(|coder| Coder {
                    reached: call.checkpoint.clone(),
                    ..coder
                })
        };
        let scopes = coder_scopes(&entries);
        let mut outcomes: Vec<Option<CoderOutcome>> = vec![None; size];
        for (index, outcome) in left.returned.iter().filter_map(outcome_of) {
            if let Some(slot) = outcomes.get_mut(index) {
                *slot = Some(outcome);
            }
        }
        let project = self
            .project_store
            .get(self.service.agent_id.as_str(), &plan.spec.project)?
            .unwrap_or_else(|| Project::new(self.service.agent_id.as_str(), &plan.spec.project));
        let guard = InFlightGuard::claim(&self.in_flight, plan.spec.session_id.clone());
        let executor = self.executor(plan.spec, project, Some(wave), guard);
        lock_active().extend(scopes.iter().cloned());
        for lost in left.lost.iter().filter_map(coder) {
            executor
                .launcher
                .end_job(
                    &executor.agent_id,
                    &executor.project.name,
                    &lost.scope,
                    &lost.job(),
                )
                .await;
            let outcome = CoderOutcome::Failed(OUTCOME_LOST.to_string());
            executor.settle(lost.index, &outcome).await;
            outcomes[lost.index] = Some(outcome);
        }
        let running: Vec<Coder> = left.collect.iter().filter_map(coder).collect();
        executor.register_stop().await;
        tracing::info!(wave = %key, coders = running.len(), "taking up a fan-out wave a stopped hub left");
        spawn_wave(executor.resume(running, outcomes, plan.cards, scopes));
        Ok(())
    }
}

/// All state the detached executor needs, captured before spawning (the
/// task-locals it derives from — identity, session — are gone in the task).
struct Executor {
    service: Arc<AgentService>,
    agent_id: String,
    profile: DriverProfile,
    launcher: Arc<dyn SandboxLauncher>,
    engine: Arc<dyn CodingEngine>,
    project: Project,
    tokens: std::collections::HashMap<String, String>,
    gh_token: Option<String>,
    spec: WaveSpec,
    token: CancellationToken,
    work_id: String,
    /// The detached wave's journal: `None` for an inline wave, which the
    /// turn that runs it answers for.
    wave: Option<Wave>,
    /// Held for the WHOLE run (channel: until `run` finishes; inline: until
    /// `execute` returns), so a second fan-out for this thread is rejected for
    /// the entire detached wave — not just until the ack returns.
    _guard: Option<InFlightGuard>,
}

impl Executor {
    /// One coder per subtask, each with a round of its own, its scope
    /// registered in [`ACTIVE_SCOPES`] BEFORE anything exists for it, so a
    /// concurrent fan-out's orphan sweep can never reap it.
    fn provision(&self) -> Vec<Coder> {
        let coders: Vec<Coder> = self
            .spec
            .subtasks
            .iter()
            .enumerate()
            .map(|(index, instruction)| Coder {
                index,
                instruction: instruction.clone(),
                scope: format!("{SCOPE_PREFIX}{}", Uuid::new_v4()),
                key: RoundKey::new(),
                reached: None,
            })
            .collect();
        lock_active().extend(coders.iter().map(|c| c.scope.clone()));
        coders
    }

    /// Journal the wave's plan and its coders', so a replica can take the
    /// wave up if this one stops. A wave that cannot be journalled runs on as
    /// one a restart may lose, still holding its journal so that nobody
    /// takes up the part that was.
    async fn record(&self, posted: &[(Arc<dyn ChannelSurface>, String)], coders: &[Coder]) {
        let Some(wave) = &self.wave else {
            return;
        };
        let plan = WavePlan {
            spec: self.spec.clone(),
            cards: posted
                .iter()
                .map(|(surface, message_id)| Card {
                    platform: surface.platform().to_string(),
                    message_id: message_id.clone(),
                })
                .collect(),
        };
        let recorded = async {
            wave.journal
                .pending(
                    WAVE_CALL,
                    WAVE_TOOL,
                    &Value::Null,
                    serde_json::to_value(&plan)?,
                )
                .await?;
            for coder in coders {
                wave.journal
                    .pending(
                        &coder_call(coder.index),
                        CODER_TOOL,
                        &Value::Null,
                        serde_json::to_value(coder)?,
                    )
                    .await?;
            }
            Ok::<_, eyre::Report>(())
        };
        if let Err(e) = recorded.await {
            tracing::warn!(error = %e, "a fan-out wave could not be journalled; a restart may lose it");
        }
    }

    /// Journal what a coder came to, so a replica taking the wave up does not
    /// run it again.
    async fn settle(&self, index: usize, outcome: &CoderOutcome) {
        let Some(wave) = &self.wave else {
            return;
        };
        let invocation = ToolInvocation {
            tool_name: CODER_TOOL.to_string(),
            input: Value::Null,
            output: serde_json::to_value(outcome).expect("an outcome serialises"),
            success: !matches!(outcome, CoderOutcome::Failed(_)),
            policy_decision: "allow".to_string(),
            duration_ms: 0,
            denial_reason: None,
            error: None,
        };
        wave.journal.finished(&coder_call(index), &invocation).await;
    }

    /// Make the wave stoppable from its thread.
    async fn register_stop(&self) {
        let Some(channel) = self.spec.channel.clone() else {
            return;
        };
        self.service.background_work.write().await.insert(
            self.work_id.clone(),
            BackgroundWork {
                target: CancelTarget::Token(self.token.clone()),
                channel,
                thread: self.spec.thread.clone(),
                requester: self.spec.requester.clone(),
                label: format!("coding fan-out: {} subtasks", self.spec.subtasks.len()),
            },
        );
    }

    /// Drive one coder: claim a shared Claude Code concurrency permit (the same
    /// per-agent + hub-wide budget the advisor draws from, so the two surfaces
    /// can't jointly exceed the host ceiling), materialise its workspace and
    /// run the setup there, build an ephemeral CC config, run it
    /// (cancel-aware), and map its reply to an outcome. The permit is held for
    /// the subprocess and released on return or future-drop (cancel/deadline),
    /// so provisioning runs inside the wave's parallelism and deadline like the
    /// coding it precedes. On a worker that keeps its jobs, a journalled
    /// wave's coder runs keyed, from the job it reached when collected.
    async fn run_coder(&self, coder: &Coder) -> CoderOutcome {
        let Some(_permits) = crate::cc_budget::acquire(&self.service, Some(&self.token)).await
        else {
            return CoderOutcome::Failed("cancelled before start".to_string());
        };
        let sandbox = match self
            .launcher
            .ensure(&WorkspaceSpec {
                agent_id: &self.agent_id,
                project: &self.project,
                scope: &coder.scope,
                tokens: &self.tokens,
                home: HomeSpec::Ephemeral,
                primary: None,
            })
            .await
        {
            Ok(sandbox) => sandbox,
            Err(e) => return CoderOutcome::Failed(format!("workspace provision failed: {e}")),
        };
        let kept = sandbox.placement().is_some_and(|placement| placement.jobs);
        let jobs = match (self.wave.as_ref().filter(|_| kept), coder.jobs()) {
            (Some(_), Some(jobs)) => Some(jobs),
            _ if coder.reached.is_some() => {
                return CoderOutcome::Failed(OUTCOME_LOST.to_string());
            }
            _ => None,
        };
        if let (Some(wave), None) = (&self.wave, &jobs) {
            if let Err(e) = wave
                .journal
                .checkpoint(&coder_call(coder.index), UNKEPT)
                .await
            {
                tracing::warn!(error = %e, "journalling a fan-out coder a restart would lose");
            }
        }
        let round = coding_engine::CodingRound {
            max_turns: Some(self.profile.budget.max_turns),
            model: self
                .spec
                .model
                .clone()
                .or_else(|| self.profile.model.clone()),
            effort: self
                .spec
                .effort
                .clone()
                .or_else(|| self.profile.effort.clone()),
            tools: CODING_BASE_TOOLS.iter().map(|s| s.to_string()).collect(),
            system_prompt: Some(format!(
                "{}\n\n{}",
                driver_append_prompt(
                    self.profile.budget,
                    Workspace::ProjectRepos,
                    &self.profile.preamble,
                    sandbox.setup(),
                ),
                fanout_directive(scope_short(&coder.scope))
            )),
            memory: coding_engine::MemoryLoad {
                claude_md: true,
                auto_memory: false,
            },
            gh_token: self.gh_token.clone(),
            cache_dir: self.profile.cache_dir.clone(),
            jobs: jobs.clone(),
            ..coding_engine::CodingRound::one_shot(
                coder.instruction.clone(),
                self.profile.budget.wall_clock,
            )
        };
        let ctx = RoundContext {
            service: &self.service,
            identity: self.spec.identity.as_deref(),
            stand_in: SCHEDULER,
            ticket_id: None,
            description: "coding.fanout",
            resume: crate::mcp_gate::RouteResume::Nowhere,
        };
        let run = self.engine.run(
            Arc::clone(&sandbox),
            &round,
            &ctx,
            Some(self.token.clone()),
            None,
        );
        let result = match (&self.wave, jobs) {
            (Some(wave), Some(_)) => {
                let journal: Arc<dyn TurnJournal> = wave.journal.clone();
                ToolCall::resumed(coder_call(coder.index), Some(journal), Value::Null)
                    .scope(run)
                    .await
            }
            _ => run.await,
        };
        outcome_from_result(&result)
    }

    /// The scopes of every coder an open wave's journal holds.
    fn journalled_scopes(&self) -> HashSet<String> {
        let sessions = &self.service.sessions;
        sessions
            .journals(SCOPE_PREFIX)
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "listing fan-out waves before a sweep");
                Vec::new()
            })
            .iter()
            .filter_map(|key| sessions.journal(key).ok())
            .flat_map(|entries| coder_scopes(&entries))
            .collect()
    }

    /// Tear down every registered workspace, sequentially and only after the
    /// whole wave has finished — removing a worktree writes the shared base's
    /// registry, so doing it while other coders still commit would re-open the
    /// base race. Each scope is also deregistered from [`ACTIVE_SCOPES`].
    async fn cleanup(&self, scopes: &[String]) {
        for scope in scopes {
            if let Err(e) = self
                .launcher
                .close(&self.agent_id, &self.project.name, scope)
                .await
            {
                tracing::warn!(scope, error = %e, "Failed to remove a fan-out workspace");
            }
            lock_active().remove(scope);
        }
    }

    /// Overall backstop so a stuck wave never hangs forever: bounded by the
    /// number of waves × the per-coder ceiling, plus slack.
    fn deadline(&self) -> Duration {
        let waves = self
            .spec
            .subtasks
            .len()
            .div_ceil(self.spec.max_parallel.max(1)) as u64;
        Duration::from_secs(waves * (self.profile.budget.wall_clock.as_secs() + 120) + 120)
    }

    /// Run `coders`, optionally editing `posted` progress as each finishes,
    /// then tear down `scopes`. Returns index-aligned outcomes, those in
    /// `outcomes` already kept (a leftover `None` — only on the overall
    /// deadline backstop — maps to a failure).
    async fn run_wave(
        &self,
        coders: Vec<Coder>,
        mut outcomes: Vec<Option<CoderOutcome>>,
        posted: Option<&[(Arc<dyn ChannelSurface>, String)]>,
        scopes: &[String],
    ) -> Vec<CoderOutcome> {
        // Sweep true orphans only. Liveness is checked per entry (not a pre-scan
        // snapshot): a concurrent fan-out registers its scope in `ACTIVE_SCOPES`
        // before the worktree hits disk, so a live scope observed on disk is
        // always seen as active here and spared. A wave a stopped replica left
        // keeps its scopes for whoever takes it up.
        let journalled = self.journalled_scopes();
        self.launcher
            .sweep(
                &self.agent_id,
                &self.project.name,
                SCOPE_PREFIX,
                &|scope: &str| lock_active().contains(scope) || journalled.contains(scope),
            )
            .await;

        // Move each owned `Coder` into its future (a closure taking `&Coder` and
        // returning a future borrowing it can't satisfy the higher-ranked
        // lifetime bound `buffer_unordered` needs).
        let mut stream = stream::iter(
            coders
                .into_iter()
                .map(|coder| async move { (coder.index, self.run_coder(&coder).await) }),
        )
        .buffer_unordered(self.spec.max_parallel.max(1));

        let drain = async {
            while let Some((index, outcome)) = stream.next().await {
                self.settle(index, &outcome).await;
                outcomes[index] = Some(outcome);
                if let Some(posted) = posted {
                    self.edit(posted, render_progress(&self.spec.subtasks, &outcomes))
                        .await;
                }
            }
        };
        let timed_out = tokio::time::timeout(self.deadline(), drain).await.is_err();
        drop(stream);
        if timed_out {
            for (index, slot) in outcomes.iter_mut().enumerate() {
                if slot.is_none() {
                    let outcome = CoderOutcome::Failed("exceeded the fan-out deadline".to_string());
                    self.settle(index, &outcome).await;
                    *slot = Some(outcome);
                }
            }
        }

        self.cleanup(scopes).await;
        outcomes
            .into_iter()
            .map(|o| o.unwrap_or(CoderOutcome::Failed("did not run".to_string())))
            .collect()
    }

    /// Channel path: post progress, journal the wave, run it, and finish.
    /// Detached after the ack.
    async fn run(self) {
        let posted = self
            .post(render_progress(
                &self.spec.subtasks,
                &vec![None; self.spec.subtasks.len()],
            ))
            .await;
        let coders = self.provision();
        self.record(&posted, &coders).await;
        let scopes: Vec<String> = coders.iter().map(|c| c.scope.clone()).collect();
        let outcomes = vec![None; self.spec.subtasks.len()];
        self.finish(coders, outcomes, &posted, &scopes).await;
    }

    /// A wave taken up from its journal: find its cards again, run the
    /// coders it left, and finish.
    async fn resume(
        self,
        coders: Vec<Coder>,
        outcomes: Vec<Option<CoderOutcome>>,
        cards: Vec<Card>,
        scopes: Vec<String>,
    ) {
        let progress = render_progress(&self.spec.subtasks, &outcomes);
        let mut posted = self.find(&cards, progress.clone()).await;
        if posted.is_empty() {
            posted = self.post(progress).await;
        }
        self.finish(coders, outcomes, &posted, &scopes).await;
    }

    /// Run `coders`, deliver the summary (or a "stopped" card if cancelled),
    /// deregister, and let the journal go.
    async fn finish(
        &self,
        coders: Vec<Coder>,
        outcomes: Vec<Option<CoderOutcome>>,
        posted: &[(Arc<dyn ChannelSurface>, String)],
        scopes: &[String],
    ) {
        let outcomes = self.run_wave(coders, outcomes, Some(posted), scopes).await;
        let final_text = if self.token.is_cancelled() {
            ":no_entry_sign: *Coding fan-out stopped at your request.*".to_string()
        } else {
            render_progress(
                &self.spec.subtasks,
                &outcomes.iter().cloned().map(Some).collect::<Vec<_>>(),
            )
        };
        self.edit(posted, final_text).await;
        self.service
            .background_work
            .write()
            .await
            .remove(&self.work_id);
        if let Some(wave) = &self.wave {
            wave.journal.clear();
        }
    }

    async fn post(&self, text: String) -> Vec<(Arc<dyn ChannelSurface>, String)> {
        let Some(channel) = self.spec.channel.clone() else {
            return Vec::new();
        };
        let message = OutboundMessage {
            channel,
            text,
            thread_id: self.spec.thread.clone(),
            username: None,
            icon_url: None,
            blocks: None,
        };
        let mut posted = Vec::new();
        for surface in self.service.surface_registry.read().await.all() {
            match surface.send_message(&message).await {
                Ok(sent) => posted.push((surface, sent.message_id)),
                Err(e) => {
                    tracing::warn!(platform = %surface.platform(), error = %e, "Failed to post coding fan-out progress")
                }
            }
        }
        posted
    }

    /// The surfaces `cards` were posted through, each found by the edit to
    /// `text` it accepts: a surface of the same platform in another
    /// workspace refuses a message it does not hold. A wave none of whose
    /// cards can be found posts a fresh one.
    async fn find(&self, cards: &[Card], text: String) -> Vec<(Arc<dyn ChannelSurface>, String)> {
        let Some(channel) = self.spec.channel.as_deref() else {
            return Vec::new();
        };
        let registry = self.service.surface_registry.read().await;
        let mut found = Vec::new();
        for card in cards {
            let mut holder = None;
            for surface in registry.all_for(&card.platform) {
                if surface
                    .edit_message(channel, &card.message_id, &text)
                    .await
                    .is_ok()
                {
                    holder = Some(surface);
                    break;
                }
            }
            match holder {
                Some(surface) => found.push((surface, card.message_id.clone())),
                None => tracing::warn!(
                    platform = %card.platform,
                    "a fan-out card could not be found again; its progress goes unreported"
                ),
            }
        }
        found
    }

    async fn edit(&self, posted: &[(Arc<dyn ChannelSurface>, String)], text: String) {
        let Some(channel) = self.spec.channel.as_deref() else {
            return;
        };
        for (surface, message_id) in posted {
            if let Err(e) = surface.edit_message(channel, message_id, &text).await {
                tracing::warn!(platform = %surface.platform(), error = %e, "Failed to update coding fan-out progress");
            }
        }
    }
}

#[async_trait]
impl Tool for CodingFanoutTool {
    fn name(&self) -> &str {
        "coding.fanout"
    }

    fn description(&self) -> &str {
        "Divide a coding job into INDEPENDENT subtasks and implement them in parallel — each \
         subtask runs as its own Claude Code coder in an isolated worktree and opens its OWN PR. \
         Provide the project and 2+ subtasks; each must be self-contained and touch DISJOINT files \
         (the coders run blind to each other, so overlapping or dependent work collides). Use this \
         for genuinely parallel work (independent tickets, disjoint modules, per-file test \
         writing); for one coupled feature where steps build on each other, use `coding.session` \
         instead. It posts progress and the resulting PR links back here. The project must already \
         exist (projects.create / projects.add_repo)."
    }

    fn input_schema(&self) -> Value {
        let choices = self.engine.model_choices();
        json!({
            "type": "object",
            "properties": {
                "project": { "type": "string", "description": "Name of the project (must already exist for this agent) whose repos the coders work in." },
                "subtasks": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "2+ INDEPENDENT instructions, each a self-contained chunk of work touching disjoint files. Each becomes one parallel coder that opens its own PR. Dependent or overlapping work does NOT belong here — use coding.session."
                },
                "model": crate::coding_session_tool::model_choice_schema(&choices, "the model every coder runs on (default: the engine's default)"),
                "effort": crate::coding_session_tool::effort_choice_schema(&choices, "the reasoning effort every coder runs at (default: medium)")
            },
            "required": ["project", "subtasks"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid input: {e}"),
                    elapsed(),
                ))
            }
        };

        let Some(session_id) = current_session_id() else {
            return Ok(ToolResult::failure(
                "coding.fanout must be called inside an agent turn (no session in scope).",
                elapsed(),
            ));
        };
        let Some(guard) = InFlightGuard::claim(&self.in_flight, session_id) else {
            return Ok(ToolResult::failure(
                "A coding.fanout run is already in progress for this thread; wait for it to finish.",
                elapsed(),
            ));
        };

        let (subtasks, dropped) = sanitise_subtasks(input.subtasks);
        if subtasks.len() < MIN_SUBTASKS {
            return Ok(ToolResult::failure(
                format!(
                    "coding.fanout needs at least {MIN_SUBTASKS} independent subtasks; for a single \
                     piece of work use coding.session.",
                ),
                elapsed(),
            ));
        }

        let identity_id = crate::user_config::current_identity_id();
        if let Err(reason) = self.engine.preflight(&RoundContext {
            service: &self.service,
            identity: identity_id.as_deref(),
            stand_in: SCHEDULER,
            ticket_id: None,
            description: "coding.fanout",
            resume: crate::mcp_gate::RouteResume::Nowhere,
        }) {
            return Ok(ToolResult::failure(reason.reason, elapsed()));
        }

        let project = match self
            .project_store
            .get(self.service.agent_id.as_str(), &input.project)
        {
            Ok(Some(p)) if !p.repos.is_empty() => p,
            Ok(Some(_)) => {
                return Ok(ToolResult::failure(
                    format!(
                        "Project `{}` has no repos yet. Add some with projects.add_repo, then retry.",
                        input.project
                    ),
                    elapsed(),
                ))
            }
            Ok(None) => {
                return Ok(ToolResult::failure(
                    format!(
                        "Project `{}` does not exist for this agent. Create it with projects.create and projects.add_repo first.",
                        input.project
                    ),
                    elapsed(),
                ))
            }
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to load project `{}`: {e}", input.project),
                    elapsed(),
                ))
            }
        };

        let (channel, thread) = self.service.current_messaging_context().await;
        let spec = WaveSpec {
            session_id: guard.session_id().to_string(),
            project: project.name.clone(),
            identity: identity_id,
            requester: CURRENT_USER_ID.try_with(|u| u.clone()).ok(),
            model: input.model,
            effort: input.effort,
            max_parallel: self
                .service
                .config
                .coding_session
                .effective_fanout_max_parallel(),
            subtasks: subtasks.clone(),
            channel: channel.clone(),
            thread,
        };

        // No channel (e.g. a gateway session) — run inline and return the
        // per-subtask outcomes, since there's no async surface to deliver to.
        if channel.is_none() {
            let executor = self.executor(spec, project, None, Some(guard));
            let coders = executor.provision();
            let scopes: Vec<String> = coders.iter().map(|c| c.scope.clone()).collect();
            let outcomes = executor
                .run_wave(coders, vec![None; subtasks.len()], None, &scopes)
                .await;
            return Ok(ToolResult::success(
                results_json(&subtasks, &outcomes, dropped, "complete"),
                elapsed(),
            ));
        }

        let wave = Wave::hold(
            &self.service.sessions,
            &format!("{SCOPE_PREFIX}{}", Uuid::new_v4()),
        )
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "a fan-out wave could not be claimed; a restart will lose it");
            None
        });
        let executor = self.executor(spec, project, wave, Some(guard));
        executor.register_stop().await;
        spawn_wave(executor.run());

        Ok(ToolResult::success(
            json!({
                "status": "running",
                "subtasks": subtasks.len(),
                "dropped": dropped,
                "message": format!(
                    "Fanning out {} subtasks in parallel — I'll post progress and the PR links here.",
                    subtasks.len()
                ),
            }),
            elapsed(),
        ))
    }
}

/// The current workforce session id, or `None` outside an agent turn.
fn current_session_id() -> Option<String> {
    workforce_agent::runner::CURRENT_SESSION_ID
        .try_with(|s| s.clone())
        .ok()
        .filter(|s| !s.is_empty())
}

fn results_json(
    subtasks: &[String],
    outcomes: &[CoderOutcome],
    dropped: usize,
    status: &str,
) -> Value {
    let results: Vec<Value> = subtasks
        .iter()
        .zip(outcomes)
        .map(|(instruction, outcome)| match outcome {
            CoderOutcome::Pr(url) => {
                json!({ "subtask": instruction, "status": "pr_opened", "pr_url": url })
            }
            CoderOutcome::NoPr(detail) => {
                json!({ "subtask": instruction, "status": "no_pr", "detail": detail })
            }
            CoderOutcome::Failed(reason) => {
                json!({ "subtask": instruction, "status": "failed", "error": reason })
            }
        })
        .collect();
    json!({ "status": status, "dropped": dropped, "results": results })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::Compose as _;

    #[test]
    fn sanitise_trims_drops_blanks_and_caps() {
        let mut raw = vec![
            "  a ".to_string(),
            String::new(),
            "  ".to_string(),
            "b".to_string(),
        ];
        raw.extend((0..20).map(|i| format!("task {i}")));
        let (kept, dropped) = sanitise_subtasks(raw);
        assert_eq!(kept.len(), MAX_SUBTASKS);
        assert_eq!(kept[0], "a");
        assert_eq!(kept[1], "b");
        assert_eq!(dropped, 22 - MAX_SUBTASKS);
    }

    #[test]
    fn sanitise_reports_no_drop_under_cap() {
        let (kept, dropped) = sanitise_subtasks(vec!["a".into(), "b".into()]);
        assert_eq!(kept, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn scope_short_takes_uuid_prefix() {
        assert_eq!(scope_short("fanout-0123456789abcdef"), "01234567");
        assert_eq!(scope_short("short"), "short");
    }

    #[test]
    fn outcome_from_result_maps_pr_done_and_failure() {
        let pr = Ok(claude_code::ClaudeCodeResult {
            engine: String::new(),
            model: None,
            text: "did it\nSTATUS: DONE\nPR_URL: https://github.com/o/r/pull/9".into(),
            exit_code: 0,
            ..Default::default()
        });
        assert_eq!(
            outcome_from_result(&pr),
            CoderOutcome::Pr("https://github.com/o/r/pull/9".to_string())
        );

        let blocked = Ok(claude_code::ClaudeCodeResult {
            engine: String::new(),
            model: None,
            text: "need a decision\nSTATUS: BLOCKED".into(),
            exit_code: 0,
            ..Default::default()
        });
        assert_eq!(
            outcome_from_result(&blocked),
            CoderOutcome::NoPr("BLOCKED".to_string())
        );

        let failed = Ok(claude_code::ClaudeCodeResult {
            engine: String::new(),
            model: None,
            text: String::new(),
            exit_code: 1,
            completion: claude_code::Completion::Failed,
            ..Default::default()
        });
        assert!(matches!(
            outcome_from_result(&failed),
            CoderOutcome::Failed(_)
        ));

        // A coder that ran out of budget still pushed real work — its PR must
        // be reported, not swallowed as a failure.
        let capped = Ok(claude_code::ClaudeCodeResult {
            engine: String::new(),
            model: None,
            text: "ran long\nPR_URL: https://github.com/o/r/pull/11".into(),
            exit_code: 1,
            completion: claude_code::Completion::TurnCap,
            ..Default::default()
        });
        assert_eq!(
            outcome_from_result(&capped),
            CoderOutcome::Pr("https://github.com/o/r/pull/11".to_string())
        );
    }

    #[test]
    fn render_progress_shows_counts_links_and_states() {
        let subtasks = vec![
            "add widget".to_string(),
            "write tests".to_string(),
            "fix bug".to_string(),
        ];
        let outcomes = vec![
            Some(CoderOutcome::Pr(
                "https://github.com/o/r/pull/1".to_string(),
            )),
            Some(CoderOutcome::Failed("boom".to_string())),
            None,
        ];
        let text = render_progress(&subtasks, &outcomes);
        assert!(text.contains("2/3 subtasks"));
        assert!(text.contains("✅ add widget → https://github.com/o/r/pull/1"));
        assert!(text.contains("❌ write tests — boom"));
        assert!(text.contains("🔄 fix bug"));
    }

    #[test]
    fn results_json_maps_each_outcome() {
        let subtasks = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let outcomes = vec![
            CoderOutcome::Pr("url".to_string()),
            CoderOutcome::NoPr("BLOCKED".to_string()),
            CoderOutcome::Failed("err".to_string()),
        ];
        let v = results_json(&subtasks, &outcomes, 1, "complete");
        assert_eq!(v["status"], "complete");
        assert_eq!(v["dropped"], 1);
        assert_eq!(v["results"][0]["status"], "pr_opened");
        assert_eq!(v["results"][0]["pr_url"], "url");
        assert_eq!(v["results"][1]["status"], "no_pr");
        assert_eq!(v["results"][2]["status"], "failed");
    }

    mod durable {
        use super::*;
        use crate::coding_testing::{Board, Rounds, Seats};
        use workforce_common::channels::SurfaceKey;
        use workforce_common::session::JournalEntry;

        struct Host {
            tool: Arc<CodingFanoutTool>,
            engine: Arc<Rounds>,
            seats: Arc<Seats>,
            board: Arc<Board>,
            sessions: Arc<dyn SessionStore>,
            /// The wave's journal key, its own so that tests running at once
            /// do not share this process's running waves.
            key: String,
        }

        async fn host() -> Host {
            host_on(Seats::default()).await
        }

        async fn host_on(seats: Seats) -> Host {
            host_with(seats, Board::default()).await
        }

        async fn host_with(seats: Seats, board: Board) -> Host {
            let tmp = tempfile::tempdir().unwrap();
            let mut config = workforce_common::settings::WorkforceConfig::default();
            config.hub.data_dir = tmp.path().to_string_lossy().to_string();
            config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
                .join("memory.db")
                .to_string_lossy()
                .into_owned();
            config.llm.api_key = Some("test-key".into());
            std::mem::forget(tmp);
            let service = AgentService::from_config(config, None).await.unwrap();
            let board = Arc::new(board);
            service
                .surface_registry
                .write()
                .await
                .register(SurfaceKey::platform("board"), board.clone());
            let projects: Arc<dyn ProjectStore> =
                Arc::new(workforce_projects::sqlite::SqliteProjectStore::in_memory().unwrap());
            projects
                .put(&Project::new(service.agent_id.as_str(), "p1"))
                .unwrap();
            let engine = Arc::new(Rounds::default());
            let seats = Arc::new(seats);
            let sessions = Arc::clone(&service.sessions);
            let tool = Arc::new(CodingFanoutTool::new(
                service,
                projects,
                DriverProfile::from(&workforce_common::settings::CodingSessionConfig::default()),
                seats.clone(),
                engine.clone(),
            ));
            Host {
                tool,
                engine,
                seats,
                board,
                sessions,
                key: format!("{SCOPE_PREFIX}{}", Uuid::new_v4()),
            }
        }

        fn spec() -> WaveSpec {
            WaveSpec {
                session_id: "s1".into(),
                project: "p1".into(),
                identity: None,
                requester: None,
                model: None,
                effort: None,
                max_parallel: 2,
                subtasks: vec!["add widget".into(), "write tests".into()],
                channel: Some("C1".into()),
                thread: Some("100.1".into()),
            }
        }

        fn coder(index: usize) -> Coder {
            Coder {
                index,
                instruction: spec().subtasks[index].clone(),
                scope: format!("{SCOPE_PREFIX}scope-{index}"),
                key: RoundKey::new(),
                reached: None,
            }
        }

        impl Host {
            fn project(&self) -> Project {
                Project::new(self.tool.service.agent_id.as_str(), "p1")
            }

            fn append(&self, call: &str, at: chrono::DateTime<chrono::Utc>, step: JournalStep) {
                self.sessions
                    .journal_append(
                        &self.key,
                        &JournalEntry {
                            call: call.into(),
                            at,
                            step,
                        },
                    )
                    .unwrap();
            }

            fn pending(
                &self,
                call: &str,
                tool: &str,
                plan: Value,
                at: chrono::DateTime<chrono::Utc>,
            ) {
                self.append(
                    call,
                    at,
                    JournalStep::Pending {
                        tool: tool.into(),
                        input: Value::Null,
                        plan,
                        actor: None,
                    },
                );
            }

            /// A wave a stopped hub left: its plan and card, and `coders`
            /// journalled at `at`, each with the job it reached.
            fn left(&self, coders: &[(Coder, Option<String>)], at: chrono::DateTime<chrono::Utc>) {
                let plan = WavePlan {
                    spec: spec(),
                    cards: vec![Card {
                        platform: "board".into(),
                        message_id: "card-7".into(),
                    }],
                };
                self.pending(
                    WAVE_CALL,
                    WAVE_TOOL,
                    serde_json::to_value(plan).unwrap(),
                    at,
                );
                for (coder, reached) in coders {
                    let call = coder_call(coder.index);
                    self.pending(&call, CODER_TOOL, serde_json::to_value(coder).unwrap(), at);
                    if let Some(mark) = reached {
                        self.append(&call, at, JournalStep::Checkpoint { mark: mark.clone() });
                    }
                }
            }

            fn finished(&self, index: usize, outcome: &CoderOutcome) {
                let row = SessionMessage {
                    id: coder_call(index),
                    ..SessionMessage::tool(
                        CODER_TOOL,
                        &Value::Null,
                        &serde_json::to_value(outcome).unwrap(),
                        true,
                        0,
                        None,
                    )
                };
                self.append(
                    &coder_call(index),
                    chrono::Utc::now(),
                    JournalStep::Finished {
                        message: Box::new(row),
                    },
                );
            }

            /// Wait for the waves taken up to finish and let their journals go.
            async fn settled(&self) {
                for _ in 0..500 {
                    if self.sessions.journals(SCOPE_PREFIX).unwrap().is_empty()
                        && !ACTIVE_WAVES.lock().unwrap().contains(&self.key)
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                panic!("the wave never finished");
            }

            fn last_edit(&self) -> String {
                self.board.edits.lock().unwrap().last().unwrap().1.clone()
            }
        }

        #[tokio::test]
        async fn a_wave_journals_its_plan_and_coders_until_it_is_done() {
            let h = host().await;
            let mid_wave = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&mid_wave);
            let sessions = Arc::clone(&h.sessions);
            let key = h.key.clone();
            *h.engine.probe.lock().unwrap() = Some(Box::new(move || {
                *seen.lock().unwrap() = sessions.journal(&key).unwrap();
            }));
            let wave = Wave::hold(&h.sessions, &h.key).unwrap();
            let executor = h.tool.executor(spec(), h.project(), wave, None);

            executor.run().await;

            let entries = mid_wave.lock().unwrap().clone();
            let plan = entries
                .iter()
                .find_map(|entry| match &entry.step {
                    JournalStep::Pending { tool, plan, .. } if tool == WAVE_TOOL => {
                        serde_json::from_value::<WavePlan>(plan.clone()).ok()
                    }
                    _ => None,
                })
                .expect("the wave's plan");
            assert_eq!(plan.cards.len(), 1);
            assert_eq!(plan.cards[0].message_id, "card-1");
            let coders = entries
                .iter()
                .filter(|entry| matches!(&entry.step, JournalStep::Pending { tool, .. } if tool == CODER_TOOL))
                .count();
            assert_eq!(coders, 2);
            assert!(entries
                .iter()
                .any(|entry| matches!(entry.step, JournalStep::Checkpoint { .. })));
            assert_eq!(h.engine.runs().len(), 2);
            assert!(h.sessions.journal(&h.key).unwrap().is_empty());
            assert_eq!(h.seats.closed.lock().unwrap().len(), 2);
            assert!(h.last_edit().contains("2/2 subtasks"));
        }

        #[tokio::test]
        async fn a_wave_a_stopped_hub_left_is_taken_up_where_its_coders_got_to() {
            let h = host().await;
            let (done, running) = (coder(0), coder(1));
            let mark = running.key.jobs(false).job().to_string();
            h.left(
                &[(done, None), (running, Some(mark.clone()))],
                chrono::Utc::now(),
            );
            h.finished(0, &CoderOutcome::Pr("https://github.com/o/r/pull/9".into()));

            h.tool.recover().await;
            h.settled().await;

            assert_eq!(h.engine.runs(), vec![(mark, true, None)]);
            let edits = h.board.edits.lock().unwrap().clone();
            assert!(edits.iter().all(|(card, _)| card == "card-7"), "{edits:?}");
            let last = h.last_edit();
            assert!(
                last.contains("2/2 subtasks") && last.contains("pull/9"),
                "{last}"
            );
            assert_eq!(h.seats.closed.lock().unwrap().len(), 2);
        }

        #[tokio::test]
        async fn a_coder_too_old_to_collect_is_ended_and_reported_unknown() {
            let h = host().await;
            let lost = coder(0);
            let mark = lost.key.jobs(false).job().to_string();
            let scope = lost.scope.clone();
            let old = chrono::Utc::now() - crate::turn_journal::COLLECT_WITHIN;
            h.left(&[(lost, Some(mark.clone())), (coder(1), None)], old);
            h.finished(1, &CoderOutcome::NoPr("BLOCKED".into()));

            h.tool.recover().await;
            h.settled().await;

            assert!(h.engine.seen.lock().unwrap().is_empty());
            assert_eq!(*h.seats.ended.lock().unwrap(), vec![(scope, mark)]);
            assert!(h.last_edit().contains("outcome is unknown"));
        }

        #[tokio::test]
        async fn a_new_wave_leaves_the_workspaces_of_one_waiting_to_be_taken_up() {
            let waiting = coder(0);
            let h = host_on(Seats {
                on_disk: vec![waiting.scope.clone(), format!("{SCOPE_PREFIX}orphan")],
                ..Default::default()
            })
            .await;
            h.left(&[(waiting, None)], chrono::Utc::now());
            let executor = h.tool.executor(spec(), h.project(), None, None);

            executor.run().await;

            assert_eq!(
                *h.seats.reaped.lock().unwrap(),
                vec![format!("{SCOPE_PREFIX}orphan")]
            );
        }

        #[tokio::test]
        async fn a_wave_whose_plan_is_unreadable_is_let_go() {
            let h = host().await;
            h.pending(
                WAVE_CALL,
                WAVE_TOOL,
                json!({"subtasks": 3}),
                chrono::Utc::now(),
            );

            h.tool.recover().await;

            assert!(h.sessions.journal(&h.key).unwrap().is_empty());
        }

        #[tokio::test]
        async fn a_wave_whose_cards_are_gone_posts_its_progress_afresh() {
            let forgetful = Board {
                forgetful: true,
                ..Default::default()
            };
            let h = host_with(Seats::default(), forgetful).await;
            h.left(&[(coder(0), None), (coder(1), None)], chrono::Utc::now());

            h.tool.recover().await;
            h.settled().await;

            let posted = h.board.posted.lock().unwrap().clone();
            assert_eq!(posted.len(), 1, "{posted:?}");
            assert!(posted[0].contains("0/2 subtasks"));
        }

        #[tokio::test]
        async fn a_coder_on_a_worker_that_does_not_keep_jobs_is_reported_rather_than_rerun() {
            let h = host_on(Seats {
                unkept: true,
                ..Default::default()
            })
            .await;
            let marks = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&marks);
            let (sessions, key) = (Arc::clone(&h.sessions), h.key.clone());
            *h.engine.probe.lock().unwrap() = Some(Box::new(move || {
                *seen.lock().unwrap() = sessions
                    .journal(&key)
                    .unwrap()
                    .into_iter()
                    .filter_map(|entry| match entry.step {
                        JournalStep::Checkpoint { mark } => Some(mark),
                        _ => None,
                    })
                    .collect();
            }));
            let wave = Wave::hold(&h.sessions, &h.key).unwrap();
            h.tool.executor(spec(), h.project(), wave, None).run().await;
            assert!(marks.lock().unwrap().iter().any(|mark| mark == UNKEPT));

            let later = host_on(Seats {
                unkept: true,
                ..Default::default()
            })
            .await;
            later.left(&[(coder(0), Some(UNKEPT.into()))], chrono::Utc::now());
            later.tool.recover().await;
            later.settled().await;
            assert!(later.engine.seen.lock().unwrap().is_empty());
            assert!(later.last_edit().contains("outcome is unknown"));
        }

        #[tokio::test]
        async fn a_wave_this_hub_is_running_is_left_alone() {
            let h = host().await;
            h.left(&[(coder(0), None), (coder(1), None)], chrono::Utc::now());
            let _running = ActiveWave::enter(&h.key).unwrap();

            h.tool.recover().await;

            assert!(h.engine.seen.lock().unwrap().is_empty());
            assert!(h.board.edits.lock().unwrap().is_empty());
            assert!(!h.sessions.journal(&h.key).unwrap().is_empty());
        }
    }

    #[test]
    fn fanout_directive_pins_the_unique_branch_prefix() {
        let d = fanout_directive("0123abcd");
        assert!(d.contains("fanout/0123abcd/"));
        assert!(d.contains("open a PR"));
    }
}
