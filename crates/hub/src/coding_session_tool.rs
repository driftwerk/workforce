//! Agent-driven Claude Code tool — `coding.session`.
//!
//! Lets an agent drive Claude Code interactively in a persistent,
//! project-bound workspace — the agent is the driver, the human stays above
//! the agent. It is flexible and agent-directed: each call's `message` says,
//! in plain language, what to do this round (investigate, review a PR, make a
//! change, or make a change and open a PR) and Claude Code does exactly that,
//! remembering context across calls; a PR is opened only when asked.
//!
//! Workspaces come from the [`crate::coding_sandbox`] launcher: one checkout
//! per repo, where `origin` is an authenticated GitHub remote and `gh` is
//! authenticated (via the injected `GH_TOKEN`) so `git push` / `gh pr create`
//! / `gh pr checkout` work with no setup — or, when no project is named, the
//! agent's bench: an empty persistent workspace with the same credentials,
//! which Claude Code clones into itself. The
//! transcript persists across calls via a driver-specific session UUID on the
//! workforce [`Session`] (distinct from the slot `/cc` uses). Each call runs
//! a bounded number of turns then returns control to the driver.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use eyre::Result;
use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;

use workforce_agent::tools::exec_runner::RunnerEndpoint;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_agent::turn_journal::{ToolCall, OUTCOME_UNKNOWN};
use workforce_common::session::{Session, SessionStore, TurnPrincipal};
use workforce_common::subscription::SCHEDULER;
use workforce_projects::{Project, ProjectStore};

use crate::claude_code::{self, transcript_lost, Resume, RoundJobs, RoundKey};
use crate::coding_engine::{
    CliRoundResult, CodingEngine, CodingRound, Continuity, MemoryLoad, RoundContext,
};
use crate::coding_sandbox::{CodingSandbox, HomeSpec, SandboxLauncher, WorkspaceSpec};
use crate::projects::setup::{SetupReport, SetupRunner};
use crate::projects::{sandbox_git, work_session, workspace_scope};
use crate::serve::AgentService;
use crate::vault::Vault;

/// What one coding round is allowed to spend, so control returns to the driver
/// after a focused chunk of work rather than the subprocess running open-endedly.
///
/// `wall_clock` is the operative bound — it is the one a waiting human feels —
/// and `max_turns` a backstop against a runaway loop. Both are told to Claude
/// Code (see [`driver_append_prompt`]) so it can wind down and report rather
/// than being cut off mid-work.
#[derive(Debug, Clone, Copy)]
pub struct DriverBudget {
    pub max_turns: u32,
    pub wall_clock: Duration,
}

impl From<&workforce_common::settings::CodingSessionConfig> for DriverBudget {
    fn from(config: &workforce_common::settings::CodingSessionConfig) -> Self {
        Self {
            max_turns: config.effective_max_turns(),
            wall_clock: config.effective_timeout(),
        }
    }
}

/// What every coding round on this install runs with, from `[coding_session]`:
/// the budget, the model and effort a call may override, the install's
/// operating notes, and where toolchain caches persist.
#[derive(Debug, Clone)]
pub struct DriverProfile {
    pub budget: DriverBudget,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub preamble: String,
    pub cache_dir: Option<PathBuf>,
}

impl From<&workforce_common::settings::CodingSessionConfig> for DriverProfile {
    fn from(config: &workforce_common::settings::CodingSessionConfig) -> Self {
        Self {
            budget: DriverBudget::from(config),
            model: config.model.clone(),
            effort: config.effort.clone(),
            preamble: config.preamble.clone(),
            cache_dir: config.cache_dir_path().map(Path::to_path_buf),
        }
    }
}

impl DriverProfile {
    /// The environment a round and its workspace setup share, so what setup
    /// installs is what the round finds.
    pub fn cache_env(&self) -> Vec<(String, String)> {
        self.cache_dir
            .as_deref()
            .map(claude_code::toolchain_cache_env)
            .unwrap_or_default()
    }

    /// How a workspace's setup commands run: through the same runner as the
    /// round, with the round's home, path and cache, bounded by its wall clock.
    pub fn setup_runner(&self, runner: Option<&RunnerEndpoint>) -> SetupRunner {
        SetupRunner {
            runner: runner.cloned(),
            env: claude_code::inherited_toolchain_env()
                .into_iter()
                .chain(self.cache_env())
                .collect(),
            timeout: self.budget.wall_clock,
        }
    }
}

/// The result field listing the files a round on this host changed, by
/// their path on this host: what the compile gate checks the agent's turn by.
pub(crate) const CHANGED_FILES: &str = "changed_files";

/// Claude Code's own file/shell tools — always available in a coding.session.
pub(crate) const CODING_BASE_TOOLS: &[&str] = &["Read", "Edit", "Write", "Bash", "Glob", "Grep"];

/// Curated, read-leaning MCP tool families exposed to Claude Code inside a
/// coding.session: ticket + code-understanding context only — NOT the agent's
/// full surface (no vault/slack/memory/scheduler). This is a focused coding
/// tool, not the agent's shell. Hub tool names are dotted, so these are
/// prefix patterns (see `mcp::matches_any`).
///
/// Scoped to families the agent's registry actually serves: `context.*` +
/// `knowledge.*` (the latter wraps the codegraph query engine —
/// `knowledge.symbol`/`graph`/`impact` give call-graph access), plus
/// `linear.*`/`jira.*` from the agent's configured adapters. The granular
/// `codegraph.*`/`impact.*`/`project.*` tools live only in the standalone
/// `workforce-local-mcp` binary, so listing them here matched nothing.
/// Rotate the driver session (drop the UUID + worktree, start cold) after this
/// many completed rounds or this much idle time, to cap the resumed-prefix
/// prompt-cache cost the way `/cc` does.
const DRIVER_ROTATE_AFTER_ROUNDS: u64 = 20;
const DRIVER_ROTATE_AFTER_IDLE_SECS: i64 = 30 * 60;

/// Whether the driver session should rotate before this round.
fn rotation_due(
    rounds: u64,
    last_reply: Option<chrono::DateTime<Utc>>,
    now: chrono::DateTime<Utc>,
) -> bool {
    rounds >= DRIVER_ROTATE_AFTER_ROUNDS
        || last_reply.is_some_and(|t| (now - t).num_seconds() >= DRIVER_ROTATE_AFTER_IDLE_SECS)
}

/// What the round's workspace actually is, which is the only thing that
/// differs between a project-bound session and a bench one. The rest of the
/// protocol — round scope, PR recipe, wind-down, sentinels — is identical,
/// because a bench that clones a repo can legitimately end in a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Workspace {
    /// One subdirectory per project repo, each with an authenticated origin.
    ProjectRepos,
    /// Empty and persistent; Claude Code brings in whatever code it needs.
    Bench { github: bool },
}

impl Workspace {
    /// Whether this round can reach GitHub at all. A bench with no token can
    /// neither clone nor push, so the protocol paragraphs that assume a remote
    /// are withheld rather than contradicting the environment.
    fn reaches_github(self) -> bool {
        !matches!(self, Self::Bench { github: false })
    }

    /// A repo-bound workspace when the project has repos; otherwise a bench,
    /// which can only reach GitHub if a token resolved for it.
    pub(crate) fn resolve(project: &Project, gh_token: Option<&str>) -> Self {
        if project.repos.is_empty() {
            Self::Bench {
                github: gh_token.is_some(),
            }
        } else {
            Self::ProjectRepos
        }
    }

    fn environment(self) -> &'static str {
        match self {
            Self::ProjectRepos => {
                "Environment: a multi-repo workspace (one subdirectory per repo). Each repo's `origin` is the \
authenticated GitHub remote and `gh` is authenticated, so `git push`, `gh pr create`, and \
`gh pr checkout <number>` work directly — never spend turns setting up git auth or remotes. \
If `git push` or `gh pr create` is rejected with a permission/403 error, the configured GitHub \
token is read-only or lacks access to that repo — return BLOCKED and say the token for that \
repo's owner needs write scope, rather than retrying or trying to reconfigure auth."
            }
            Self::Bench { github: true } => {
                "Environment: an open bench — an empty directory that persists across rounds of this \
session, with `git` and `gh` authenticated. Start new work here, or bring in existing code \
yourself with `gh repo clone <owner>/<name>` (several side by side is fine); anything you clone \
can be pushed and turned into a PR as normal. Nothing is pre-cloned, so read the instruction for \
which code it means and clone it before looking for it. The bench is working space, not \
long-term storage — it is reclaimed once left idle for long enough, so push anything that \
matters. If a clone or push is rejected with a permission/403 error, the configured GitHub token \
lacks access — return BLOCKED and say so rather than retrying or reconfiguring auth."
            }
            Self::Bench { github: false } => {
                "Environment: an open bench — an empty directory that persists across rounds of this \
session, though it is reclaimed once left idle for long enough. No GitHub token is configured, \
so `git clone`/`gh` cannot reach remote repositories: work locally in this directory, and if the \
instruction needs a repo you cannot reach, return BLOCKED saying a GitHub token needs \
configuring rather than retrying. The deliverable is the files you leave here plus your reply."
            }
        }
    }
}

/// Appended to Claude Code's system prompt: flags that it's agent-driven and
/// flexible, describes the workspace (pre-cloned project repos, or an open
/// bench it clones into itself) and what setup ran in it, carries the
/// install's own notes, and states the sentinel protocol the tool parses back
/// out of the reply.
pub(crate) fn driver_append_prompt(
    budget: DriverBudget,
    workspace: Workspace,
    preamble: &str,
    setup: &[SetupReport],
) -> String {
    let minutes = budget.wall_clock.as_secs() / 60;
    let max_turns = budget.max_turns;
    let environment = workspace.environment();
    let setup_notes = match setup {
        [] => String::new(),
        reports => format!(
            "Workspace setup ran before this round:\n{}\n\n",
            reports
                .iter()
                .map(|r| format!("- {}", r.describe()))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };
    let install_notes = match preamble.trim() {
        "" => String::new(),
        notes => format!("Notes for this install:\n{notes}\n\n"),
    };
    let round_scope = if workspace.reaches_github() {
        "investigate or answer a question about the code, review an existing PR, make a change, \
or make a change and open a pull request. Do NOT open a PR unless asked to."
    } else {
        "investigate, answer a question, or create and modify files in this workspace."
    };
    let pr_protocol = if workspace.reaches_github() {
        "When asked to open a PR: create a feature branch, commit, `git push -u origin <branch>`, then \
`gh pr create --title \"...\" --body \"...\"` with a clear description (gh defaults the base to \
the repo's default branch). Be idempotent: if a PR already exists for the branch \
(`gh pr list --head <branch> --json url -q '.[0].url'`), return that URL instead of duplicating.\n\n"
    } else {
        ""
    };
    format!(
        "\
You are driven by an orchestrating agent (not a human) that reads your reply and may send \
follow-up instructions in the same session. Do exactly what each instruction \
asks — {round_scope}\n\n\
{environment}\n\n\
{setup_notes}\
{install_notes}\
{pr_protocol}\
The round ends when you reply: never leave a background command running — wait for it or stop \
it first. In your reply say what you verified and what you left to CI or a later round.\n\n\
BUDGET: you have about {minutes} minutes of wall clock and {max_turns} turns for this round, and \
you are cut off when either runs out. Scope your work to fit, and keep an eye on how much you \
have used. As you approach either limit, stop taking on new work: commit and push anything \
worth keeping, or write it to files where there is nowhere to push, then reply with the \
sentinels summarising what you did and what remains. A wound-down reply is far more useful \
to the agent driving you than being cut off mid-work — it can send you another round to \
continue.\n\n\
Keep replies tight (≤2KB — file paths plus one line each, never full file contents or diffs). \
End every reply with these sentinels, each alone on its own line, putting any question or \
detail above them and omitting any that don't apply:\n\
STATUS: DONE          (or BLOCKED if you need a decision)\n\
BRANCH: <branch>      (if you committed)\n\
PR_URL: <url>         (if you created or found a PR)"
    )
}

/// `coding.session` — drive a persistent, GitHub-authenticated Claude Code
/// session bound to one of the agent's projects.
pub struct CodingSessionTool {
    /// The agent this tool serves; its Claude Code concurrency budget bounds
    /// every round.
    service: Arc<AgentService>,
    project_store: Arc<dyn ProjectStore>,
    sessions: Arc<dyn SessionStore>,
    /// Where a pre-migration `/cc` worktree would still be found.
    data_dir: PathBuf,
    agent_id: String,
    vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
    /// What every round runs with, from `[coding_session]`.
    profile: DriverProfile,
    /// Where a round runs, selected by `[coding_session.executor]`.
    launcher: Arc<dyn SandboxLauncher>,
    /// What runs a round, selected by `[coding_session.engine]`.
    engine: Arc<dyn CodingEngine>,
    /// Session ids with a round in flight, so concurrent calls for the same
    /// thread can't race the same `--resume` transcript / worktree.
    in_flight: Arc<Mutex<HashSet<String>>>,
}

/// RAII: drops the session id from an in-flight set on every exit path
/// (including early-return failures), so the slot is always released. Shared by
/// `coding.session` and `coding.fanout`, which each reject a second concurrent
/// round for the same thread.
pub(crate) struct InFlightGuard {
    set: Arc<Mutex<HashSet<String>>>,
    session_id: String,
}

impl InFlightGuard {
    /// Claim `session_id` in `set`; `None` if a round for it is already in
    /// flight. The returned guard releases the claim on drop.
    pub(crate) fn claim(set: &Arc<Mutex<HashSet<String>>>, session_id: String) -> Option<Self> {
        let mut in_flight = set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !in_flight.insert(session_id.clone()) {
            return None;
        }
        Some(Self {
            set: Arc::clone(set),
            session_id,
        })
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.session_id);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Input {
    #[serde(default)]
    project: Option<String>,
    message: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    new_session: bool,
}

/// A round as prepared, before anything runs: all a hub needs to drive it,
/// again after a restart if need be.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RoundPlan {
    #[serde(flatten)]
    key: RoundKey,
    project: String,
    uuid: Uuid,
    resumed: bool,
    /// Who the round runs as, which a later turn collecting it may not be.
    identity: Option<String>,
    /// Whom the round bills apart from who it runs as, whose plan pays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bill_to: Option<String>,
}

impl RoundPlan {
    fn principal(&self) -> TurnPrincipal {
        TurnPrincipal {
            run_as: self.identity.clone(),
            bill_to: self.bill_to.clone(),
        }
    }
}

/// One run of a round on the engine: the round's first attempt at its driver
/// session, or the cold restart that replaces it.
struct Leg<'a> {
    project: &'a Project,
    session_id: &'a str,
    tokens: &'a HashMap<String, String>,
    gh_token: Option<String>,
    workspace: Workspace,
    uuid: Uuid,
    resumed: bool,
    input: &'a Input,
    jobs: RoundJobs,
    /// The plan to journal once the leg's sandbox turns out to keep its jobs
    /// through a restart; a round whose jobs it does not keep has nothing a
    /// later turn could collect.
    journal: Option<&'a RoundPlan>,
    /// Whether the agent's compile gate checks what the leg changes.
    gated: bool,
}

/// A leg that ran: where, how it ended, and, on this host for an agent with
/// a compile gate, where the workspace's checkouts stood before it.
struct Ran {
    sandbox: Arc<dyn CodingSandbox>,
    before: Option<Vec<sandbox_git::Revision>>,
    result: Result<CliRoundResult>,
}

impl Ran {
    /// The files the leg changed, by their path on this host; none for a
    /// leg on a remote worker, whose files this host cannot see.
    async fn changed_files(&self) -> Vec<String> {
        let Some(before) = &self.before else {
            return Vec::new();
        };
        sandbox_git::changed_since(self.sandbox.as_ref(), before)
            .await
            .into_iter()
            .map(|file| {
                self.sandbox
                    .workdir()
                    .join(file)
                    .components()
                    .collect::<PathBuf>()
                    .display()
                    .to_string()
            })
            .collect()
    }
}

const NO_SESSION: &str =
    "coding.session must be called inside an agent turn (no session in scope).";
const ROUND_RUNNING: &str =
    "A coding.session round is already running for this thread; wait for it to finish.";

/// The `model` property of a coding tool's schema: an enum of the names the
/// engine accepts, or free text for an engine that takes any model id.
pub(crate) fn model_choice_schema(
    choices: &crate::coding_engine::ModelChoices,
    purpose: &str,
) -> serde_json::Value {
    let mut schema = serde_json::json!({
        "type": "string",
        "description": format!("{}: {purpose}.", listed(&choices.models)),
    });
    if !choices.models.is_empty() {
        schema["enum"] = serde_json::json!(choices.models);
    }
    schema
}

/// The `effort` property of a coding tool's schema.
pub(crate) fn effort_choice_schema(
    choices: &crate::coding_engine::ModelChoices,
    purpose: &str,
) -> serde_json::Value {
    let efforts: Vec<&str> = choices.efforts.to_vec();
    serde_json::json!({
        "type": "string",
        "description": format!("{}: {purpose}.", listed(&efforts)),
        "enum": efforts,
    })
}

fn listed<S: AsRef<str>>(names: &[S]) -> String {
    match names {
        [] => "Any model id the engine accepts".to_string(),
        names => names
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// Pull a sentinel value (e.g. `PR_URL: https://…`) out of Claude Code's reply.
/// Scans from the end so the final summary wins, tolerates markdown decoration,
/// and takes the first token (the values are single words / URLs) so trailing
/// prose ("DONE (handled)") doesn't leak in. `None` when the key is absent or
/// its value is empty.
pub(crate) fn parse_sentinel(text: &str, key: &str) -> Option<String> {
    text.lines().rev().find_map(|line| {
        let rest = line
            .trim_start_matches([' ', '*', '`', '-', '>', '#'])
            .strip_prefix(key)?
            .trim_start_matches([' ', '*', '`']);
        rest.split_whitespace()
            .next()
            .map(|v| v.trim_matches(['`', '*']).to_string())
            .filter(|v| !v.is_empty())
    })
}

impl CodingSessionTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        service: Arc<AgentService>,
        project_store: Arc<dyn ProjectStore>,
        sessions: Arc<dyn SessionStore>,
        data_dir: PathBuf,
        agent_id: String,
        vault: Option<Arc<Vault>>,
        global_vault: Option<Arc<Vault>>,
        profile: DriverProfile,
        launcher: Arc<dyn SandboxLauncher>,
        engine: Arc<dyn CodingEngine>,
    ) -> Self {
        Self {
            service,
            project_store,
            sessions,
            data_dir,
            agent_id,
            vault,
            global_vault,
            profile,
            launcher,
            engine,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// The workforce session id of the current turn, from the task-local the
    /// runner scopes around tool execution. `None` outside an agent turn.
    fn current_session_id() -> Option<String> {
        workforce_agent::runner::CURRENT_SESSION_ID
            .try_with(|s| s.clone())
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// Materialise the workspace for `(project, scope)` and run the project's
    /// setup in it, mapping any provisioning failure to a user-facing message.
    /// Idempotent — a prior round in the same scope short-circuits without
    /// re-cloning or re-running setup.
    async fn provision(
        &self,
        project: &Project,
        scope: &str,
        tokens: &HashMap<String, String>,
    ) -> std::result::Result<Arc<dyn CodingSandbox>, String> {
        self.launcher
            .ensure(&WorkspaceSpec {
                agent_id: &self.agent_id,
                project,
                scope,
                tokens,
                home: HomeSpec::Ephemeral,
                primary: None,
            })
            .await
            .map_err(|e| {
                format!(
                    "Failed to prepare coding workspace for project `{}`: {e}",
                    project.name
                )
            })
    }

    /// Discard the workspace a rotated, missing or lost transcript owned — the
    /// scope keyed by its uuid. A scratch workspace is never uuid-keyed (see
    /// [`crate::projects::workspace_scope`]) so it survives; a repo-backed tree
    /// left behind by a project whose last repo was removed is reclaimed here.
    /// Best-effort.
    async fn discard_transcript_workspace(&self, project_name: &str, uuid: &str) {
        if let Err(e) = self
            .launcher
            .close(&self.agent_id, project_name, uuid)
            .await
        {
            warn!(target: "coding_session", project = project_name, uuid, error = %e, "discarding a transcript's workspace");
        }
    }

    /// One round of the driver's session: the profile and the call say how it
    /// runs, the sandbox's setup shapes what it is told.
    fn build_round(
        setup: &[SetupReport],
        uuid: Uuid,
        resume: bool,
        input: &Input,
        gh_token: Option<String>,
        profile: &DriverProfile,
        workspace: Workspace,
        jobs: Option<RoundJobs>,
    ) -> CodingRound {
        CodingRound {
            task: input.message.clone(),
            max_turns: Some(profile.budget.max_turns),
            timeout: profile.budget.wall_clock,
            model: input.model.clone().or_else(|| profile.model.clone()),
            effort: input.effort.clone().or_else(|| profile.effort.clone()),
            tools: CODING_BASE_TOOLS.iter().map(|s| s.to_string()).collect(),
            system_prompt: Some(driver_append_prompt(
                profile.budget,
                workspace,
                &profile.preamble,
                setup,
            )),
            continuity: Continuity::Session { id: uuid, resume },
            memory: MemoryLoad {
                claude_md: true,
                auto_memory: false,
            },
            gh_token,
            cache_dir: profile.cache_dir.clone(),
            jobs,
        }
    }

    fn round_context<'a>(&'a self, identity: Option<&'a str>) -> RoundContext<'a> {
        RoundContext {
            service: &self.service,
            identity,
            stand_in: SCHEDULER,
            ticket_id: None,
            description: "coding.session",
            resume: crate::mcp_gate::RouteResume::InSession,
        }
    }

    /// Everything about a round that must happen once, however often it is
    /// driven: resolve the project, rotate or reset the driver session when
    /// due, and mint and persist the driver UUID the round runs under.
    async fn prepare(
        &self,
        input: &Input,
        session_id: &str,
        principal: TurnPrincipal,
    ) -> Result<RoundPlan, String> {
        let project = match input.project.as_deref() {
            Some(name) => match self.project_store.get(&self.agent_id, name) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    return Err(format!(
                        "Project `{name}` does not exist for this agent. Create it with projects.create, or omit `project` to work in your bench workspace.",
                    ))
                }
                Err(e) => return Err(format!("Failed to load project `{name}`: {e}")),
            },
            None => crate::projects::ensure_bench_project(
                self.project_store.as_ref(),
                &self.agent_id,
            )
            .map_err(|e| format!("Failed to open the bench workspace: {e}"))?,
        };
        let github = !project.repos.is_empty();
        let mut session = self.session(session_id)?;
        if input.new_session {
            session.clear_driver_claude_session_uuid(&project.name);
        }

        // Rotate (drop the session + its worktree, start cold) after enough
        // rounds or idle time, to cap the resumed-prefix prompt-cache cost.
        if let Some(stale) = session
            .driver_claude_session_uuid(&project.name)
            .map(str::to_string)
        {
            if rotation_due(
                session.driver_chat_turn_count(&project.name),
                session.driver_chat_last_reply_at(&project.name),
                Utc::now(),
            ) {
                self.discard_transcript_workspace(&project.name, &stale)
                    .await;
                session.clear_driver_claude_session_uuid(&project.name);
            }
        }

        // If a driver UUID is persisted but its worktree is gone (a redeploy
        // wiped it, or it predates this backend — e.g. a pre-migration
        // cc_session worktree), resuming Claude Code's transcript would land in
        // an empty/mismatched tree. Start fresh instead.
        let mut resumed = false;
        if let Some(existing) = session
            .driver_claude_session_uuid(&project.name)
            .map(str::to_string)
        {
            resumed = self
                .launcher
                .exists(
                    &self.agent_id,
                    &project.name,
                    &workspace_scope(github, session_id, &existing),
                )
                .await;
            if !resumed {
                warn!(target: "coding_session", session = %session_id, stale_uuid = %existing, "driver worktree missing — starting session fresh");
                let _ = crate::projects::cc_session::cleanup(
                    &self.data_dir,
                    &self.agent_id,
                    &project.name,
                    &existing,
                )
                .await;
                self.discard_transcript_workspace(&project.name, &existing)
                    .await;
                session.clear_driver_claude_session_uuid(&project.name);
            }
        }

        let uuid = session.get_or_init_driver_claude_uuid(&project.name);
        self.persist(&session)?;
        Ok(RoundPlan {
            key: RoundKey::new(),
            project: project.name,
            uuid: Uuid::parse_str(&uuid)
                .map_err(|e| format!("Invalid driver session UUID: {e}"))?,
            resumed,
            identity: principal.run_as,
            bill_to: principal.bill_to,
        })
    }

    /// Run a prepared round to its result, from its start or, collecting it,
    /// from the job it last recorded: the round decides nothing that job's
    /// leg already decided, so no job starts twice.
    async fn drive(
        &self,
        plan: &RoundPlan,
        input: &Input,
        session_id: &str,
        ctx: &RoundContext<'_>,
        from: Option<Resume>,
        start: Instant,
    ) -> ToolResult {
        let elapsed = || start.elapsed().as_millis() as u64;
        let project = match self.project_store.get(&self.agent_id, &plan.project) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return ToolResult::failure(
                    format!("Project `{}` no longer exists.", plan.project),
                    elapsed(),
                )
            }
            Err(e) => {
                return ToolResult::failure(
                    format!("Failed to load project `{}`: {e}", plan.project),
                    elapsed(),
                )
            }
        };
        let tokens = work_session::resolve_github_tokens(
            &project,
            self.vault.as_ref(),
            self.global_vault.as_ref(),
        );
        // `gh` authenticates with one process-global token, so pick the primary
        // repo's owner; `git push` stays per-owner via the per-URL credential
        // helpers installed on each base. A bench has no repo to key an owner
        // off, so it takes the flat PAT — without one it cannot clone at all,
        // which the prompt states rather than leaving Claude to discover.
        let gh_token = project
            .repos
            .first()
            .and_then(|r| {
                let workforce_projects::RepoSource::Github { owner, .. } = &r.source;
                tokens.get(owner).cloned()
            })
            .or_else(|| {
                crate::user_config::resolve_bench_github_pat(
                    self.vault.as_deref(),
                    self.global_vault.as_deref(),
                )
            });
        let workspace = Workspace::resolve(
            &project,
            claude_code::effective_gh_token(
                gh_token.clone(),
                self.vault.as_deref(),
                self.global_vault.as_deref(),
            )
            .as_deref(),
        );
        let gated = self.service.compile_gate().is_some();
        let round = |uuid: Uuid, resumed: bool, jobs: RoundJobs, journal| Leg {
            project: &project,
            session_id,
            tokens: &tokens,
            gh_token: gh_token.clone(),
            workspace,
            uuid,
            resumed,
            input,
            jobs,
            journal,
            gated,
        };
        // The same per-agent + hub-wide Claude Code budget the advisor and
        // fan-out draw from, held from provisioning through the round so the
        // surfaces cannot jointly exceed the host ceiling.
        let _permits = crate::cc_budget::acquire(&self.service, None).await;
        let from_start = from.is_none();
        let (first, cold) = match from {
            Some(Resume { cold: true, jobs }) => (None, Some(jobs)),
            Some(Resume { cold: false, jobs }) => (Some(jobs), None),
            None => (Some(plan.key.jobs(false)), None),
        };
        let first = match first {
            Some(jobs) => {
                let leg = round(plan.uuid, plan.resumed, jobs, from_start.then_some(plan));
                match self.run_leg(leg, ctx).await {
                    Ok(ran) => Some(ran),
                    Err(e) => return ToolResult::failure(e, elapsed()),
                }
            }
            None => None,
        };
        let (uuid, resumed, ran) = match first {
            Some(ran) if !transcript_lost(&ran.result) => (plan.uuid, plan.resumed, ran),
            first => {
                // Session-lost recovery: Claude Code's on-disk transcript is
                // gone (e.g. a redeploy wiped ~/.claude). Resuming the same
                // UUID fails forever, so reset the session + worktree and
                // retry the instruction once cold.
                if first.is_some() {
                    warn!(target: "coding_session", session = %session_id, stale_uuid = %plan.uuid, "Claude Code transcript missing — restarting fresh");
                    self.discard_transcript_workspace(&project.name, &plan.uuid.to_string())
                        .await;
                }
                let uuid = plan.key.cold_uuid();
                let moved = self.session(session_id).and_then(|mut session| {
                    session.clear_driver_claude_session_uuid(&project.name);
                    session.set_driver_claude_uuid(&project.name, &uuid.to_string());
                    self.persist(&session)
                });
                if let Err(e) = moved {
                    return ToolResult::failure(e, elapsed());
                }
                let jobs = cold.unwrap_or_else(|| plan.key.jobs(true));
                match self.run_leg(round(uuid, false, jobs, None), ctx).await {
                    Ok(ran) => (uuid, false, ran),
                    Err(e) => return ToolResult::failure(e, elapsed()),
                }
            }
        };

        match &ran.result {
            Ok(r) if r.completion.is_usable() => {
                // Count the completed round (drives rotation). Best-effort: a
                // failed save only skews rotation timing, not correctness.
                if let Ok(mut session) = self.session(session_id) {
                    session.record_driver_chat_turn(Utc::now(), &project.name);
                    let _ = self.sessions.save(&session);
                }
                let mut result = serde_json::json!({
                    "response": r.text,
                    "engine": r.engine,
                    "model": r.model,
                    "project": project.name,
                    "session_uuid": uuid.to_string(),
                    "resumed": resumed,
                    "turns": r.num_turns,
                    "duration_secs": r.duration.as_secs(),
                    "completion": r.completion.as_str(),
                    "truncated": r.completion.is_truncated(),
                    "status": parse_sentinel(&r.text, "STATUS:"),
                    "branch": parse_sentinel(&r.text, "BRANCH:"),
                    "pr_url": parse_sentinel(&r.text, "PR_URL:"),
                });
                if !ran.sandbox.setup().is_empty() {
                    result["setup"] = serde_json::json!(ran.sandbox.setup());
                }
                let changed = ran.changed_files().await;
                if !changed.is_empty() {
                    result[CHANGED_FILES] = serde_json::json!(changed);
                }
                ToolResult::success(result, elapsed())
            }
            _ => {
                let reason = claude_code::classify_failure(&ran.result)
                    .map(|f| f.human_reason())
                    .unwrap_or_else(|| "Claude Code failed for an unknown reason.".to_string());
                ToolResult::failure(reason, elapsed())
            }
        }
    }

    /// Provision a leg's workspace and run it on the engine.
    async fn run_leg(&self, leg: Leg<'_>, ctx: &RoundContext<'_>) -> Result<Ran, String> {
        let scope = workspace_scope(
            !leg.project.repos.is_empty(),
            leg.session_id,
            &leg.uuid.to_string(),
        );
        let sandbox = self.provision(leg.project, &scope, leg.tokens).await?;
        let kept = sandbox.placement().is_some_and(|placement| placement.jobs);
        if let (Some(plan), true, Some(call)) = (leg.journal, kept, ToolCall::current()) {
            let plan = serde_json::to_value(plan).map_err(|e| e.to_string())?;
            call.pending(Tool::name(self), plan).await.map_err(|e| {
                format!("Could not record the round before starting it, so it was not started: {e}")
            })?;
        }
        let round = Self::build_round(
            sandbox.setup(),
            leg.uuid,
            leg.resumed,
            leg.input,
            leg.gh_token,
            &self.profile,
            leg.workspace,
            Some(leg.jobs),
        );
        let before = match (leg.gated, sandbox.placement()) {
            (true, None) => Some(sandbox_git::revisions(sandbox.as_ref()).await),
            _ => None,
        };
        let result = self
            .engine
            .run(Arc::clone(&sandbox), &round, ctx, None, None)
            .await;
        Ok(Ran {
            sandbox,
            before,
            result,
        })
    }

    fn session(&self, session_id: &str) -> Result<Session, String> {
        match self.sessions.get(session_id) {
            Ok(Some(s)) => Ok(s),
            Ok(None) => Err(
                "Current session not found; cannot start or resume a coding session.".to_string(),
            ),
            Err(e) => Err(format!("Failed to load session: {e}")),
        }
    }

    /// Persist the driver UUID (the state-of-record write) — the UUID must
    /// reach the store before we run, or a later call would resume a stale UUID
    /// and orphan this round's worktree.
    fn persist(&self, session: &Session) -> Result<(), String> {
        self.sessions
            .save(session)
            .map_err(|e| format!("Failed to persist driver session UUID: {e}"))
    }
}

#[async_trait]
impl Tool for CodingSessionTool {
    fn name(&self) -> &str {
        "coding.session"
    }

    fn description(&self) -> &str {
        "Drive Claude Code in a persistent workspace. Omit `project` and you get your bench: a \
         private workspace that persists across calls, with git and gh authenticated, where \
         Claude Code can build something new (a prototype, a script, a WASM skill) or clone any \
         repo you have access to and work on it — nothing has to be set up first. Name a \
         `project` instead when you want its repos pre-cloned side by side. Either way, when you \
         ask it to open a PR it pushes and runs `gh pr create` itself and returns `pr_url` — you \
         do NOT need github.* tools or code.git for that. Say in plain language what you want \
         this round — investigate, review a PR, make a change, build something — and it does \
         exactly that, remembering context across calls (brief it, review the reply, send the \
         next instruction). NOTE: the first call against a project provisions the workspace \
         (clones its repos) and can take a minute or two; later calls are fast. A result with \
         `truncated: true` ran out of its turn or time budget with real work already done (often \
         committed and pushed) — read `response` for what landed and send a follow-up call to \
         carry on from there; never restart the task from scratch. If it fails (rate limit, \
         error), report to the user — do NOT attempt the coding yourself or fall back to an API \
         model."
    }

    fn input_schema(&self) -> serde_json::Value {
        let choices = self.engine.model_choices();
        serde_json::json!({
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Optional. Name of an existing project whose repos Claude Code works in, pre-cloned side by side. Omit it to use your bench: a persistent workspace with git and gh authenticated, where Claude Code creates new work or clones whatever repo the task needs."
                },
                "message": {
                    "type": "string",
                    "description": "What you want Claude Code to do this round, in plain language. E.g. 'investigate how X works', 'review PR #42', 'add a roadmap section to the README', or 'implement DW-123 and open a PR'. Include ticket context / acceptance criteria the first time. Ask it to open a PR only when you actually want one."
                },
                "model": model_choice_schema(&choices, "the model this round runs on (default: this install's configured model, else the engine's default)"),
                "effort": effort_choice_schema(&choices, "the reasoning effort this round runs at (default: this install's configured effort, else the engine's default)"),
                "new_session": {
                    "type": "boolean",
                    "description": "Start a fresh Claude Code session, discarding prior conversation context for this thread (scratch workspace files are kept). Also picks up repos added to the project since the session began. Default false (continue the existing session)."
                }
            },
            "required": ["message"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
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
        let Some(session_id) = Self::current_session_id() else {
            return Ok(ToolResult::failure(NO_SESSION, elapsed()));
        };
        // Reject a second concurrent round for the same thread — two parallel
        // `--resume` invocations would race the one Claude Code session. Held
        // until execute returns (any path) via the RAII guard.
        let Some(_guard) = InFlightGuard::claim(&self.in_flight, session_id.clone()) else {
            return Ok(ToolResult::failure(ROUND_RUNNING, elapsed()));
        };
        let identity_id = crate::user_config::current_identity_id();
        let ctx = self.round_context(identity_id.as_deref());
        // Fail fast before the expensive workspace provision when the engine
        // cannot serve this user (identity is in scope, so per-user tokens
        // resolve).
        if let Err(refusal) = self.engine.preflight(&ctx) {
            return Ok(ToolResult::failure(refusal.reason, elapsed()));
        }
        let principal = crate::user_config::current_principal();
        let plan = match self.prepare(&input, &session_id, principal).await {
            Ok(plan) => plan,
            Err(reason) => return Ok(ToolResult::failure(reason, elapsed())),
        };
        Ok(self
            .drive(&plan, &input, &session_id, &ctx, None, start)
            .await)
    }

    async fn collect(
        &self,
        input: serde_json::Value,
        plan: serde_json::Value,
        checkpoint: Option<String>,
    ) -> Result<ToolResult> {
        let start = Instant::now();
        let (input, plan) = match (
            serde_json::from_value::<Input>(input),
            serde_json::from_value::<RoundPlan>(plan),
        ) {
            (Ok(input), Ok(plan)) => (input, plan),
            (Err(e), _) | (_, Err(e)) => {
                return Ok(ToolResult::failure(
                    format!("The interrupted round cannot be read: {e}"),
                    0,
                ))
            }
        };
        let from = match checkpoint.as_deref().map(|mark| plan.key.resume_at(mark)) {
            Some(Some(resume)) => Some(resume),
            Some(None) => return Ok(ToolResult::failure(OUTCOME_UNKNOWN, 0)),
            None => None,
        };
        let Some(session_id) = Self::current_session_id() else {
            return Ok(ToolResult::failure(NO_SESSION, 0));
        };
        let Some(_guard) = InFlightGuard::claim(&self.in_flight, session_id.clone()) else {
            return Ok(ToolResult::failure(ROUND_RUNNING, 0));
        };
        let ctx = self.round_context(plan.identity.as_deref());
        let drive = self.drive(&plan, &input, &session_id, &ctx, from, start);
        Ok(crate::user_config::as_principal(&plan.principal(), drive).await)
    }

    async fn abandon(&self, plan: serde_json::Value, checkpoint: Option<String>) {
        let Ok(plan) = serde_json::from_value::<RoundPlan>(plan) else {
            return;
        };
        let Some(Resume { cold, jobs }) = checkpoint
            .as_deref()
            .and_then(|mark| plan.key.resume_at(mark))
        else {
            return;
        };
        let (Some(session_id), Ok(Some(project))) = (
            Self::current_session_id(),
            self.project_store.get(&self.agent_id, &plan.project),
        ) else {
            return;
        };
        let uuid = match cold {
            true => plan.key.cold_uuid(),
            false => plan.uuid,
        };
        let scope = workspace_scope(!project.repos.is_empty(), &session_id, &uuid.to_string());
        self.launcher
            .end_job(&self.agent_id, &project.name, &scope, &jobs.job())
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::Compose as _;

    use crate::coding_testing::{Rounds, Seats, ANSWER};
    use workforce_common::settings::ProgramsMode;

    #[derive(Default)]
    struct Plans {
        pending: Mutex<Vec<(serde_json::Value, serde_json::Value)>>,
        checkpoints: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl workforce_agent::turn_journal::TurnJournal for Plans {
        async fn pending(
            &self,
            _: &str,
            _: &str,
            input: &serde_json::Value,
            plan: serde_json::Value,
        ) -> Result<()> {
            self.pending.lock().unwrap().push((input.clone(), plan));
            Ok(())
        }
        async fn checkpoint(&self, _: &str, mark: &str) -> Result<()> {
            self.checkpoints.lock().unwrap().push(mark.to_string());
            Ok(())
        }
        async fn finished(&self, _: &str, _: &workforce_agent::runner::ToolInvocation) {}
    }

    struct Bench {
        tool: CodingSessionTool,
        engine: Arc<Rounds>,
        seats: Arc<Seats>,
        sessions: Arc<dyn SessionStore>,
        session_id: String,
        plans: Arc<Plans>,
    }

    async fn bench(lost: bool) -> Bench {
        bench_on(lost, Seats::default()).await
    }

    async fn bench_on(lost: bool, seats: Seats) -> Bench {
        bench_with(
            Rounds {
                lost,
                ..Default::default()
            },
            seats,
        )
        .await
    }

    async fn bench_with(engine: Rounds, seats: Seats) -> Bench {
        bench_gated(engine, seats, ProgramsMode::OptIn).await
    }

    /// A bench whose install runs the compile gate in `gate` mode.
    async fn bench_gated(engine: Rounds, seats: Seats, gate: ProgramsMode) -> Bench {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = workforce_common::settings::WorkforceConfig::default();
        config.tools.compile_gate.mode = gate;
        config.hub.data_dir = tmp.path().to_string_lossy().to_string();
        config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        config.llm.api_key = Some("test-key".into());
        let service = AgentService::from_config(config, None).await.unwrap();
        let sessions: Arc<dyn SessionStore> =
            Arc::new(workforce_common::session::InMemorySessionStore::new());
        let session_id = sessions.get_or_create("c", "o", "a").unwrap().id;
        let engine = Arc::new(engine);
        let seats = Arc::new(seats);
        let tool = CodingSessionTool::new(
            service,
            Arc::new(workforce_projects::sqlite::SqliteProjectStore::in_memory().unwrap()),
            Arc::clone(&sessions),
            tmp.path().to_path_buf(),
            "a".into(),
            None,
            None,
            DriverProfile {
                budget: budget(),
                model: None,
                effort: None,
                preamble: String::new(),
                cache_dir: None,
            },
            seats.clone(),
            engine.clone(),
        );
        std::mem::forget(tmp);
        Bench {
            tool,
            engine,
            seats,
            sessions,
            session_id,
            plans: Arc::default(),
        }
    }

    const ASK: &str = "fix it";

    impl Bench {
        /// Run `fut` as a tool call of a turn by `user`, the call's input kept
        /// as `recorded`.
        async fn in_turn<F: std::future::Future>(
            &self,
            user: &str,
            recorded: serde_json::Value,
            fut: F,
        ) -> F::Output {
            let journal: Arc<dyn workforce_agent::turn_journal::TurnJournal> = self.plans.clone();
            workforce_agent::runner::CURRENT_SESSION_ID
                .scope(
                    self.session_id.clone(),
                    workforce_agent::runner::CURRENT_USER_ID.scope(
                        user.to_string(),
                        ToolCall::new(Some(journal), recorded).scope(fut),
                    ),
                )
                .await
        }

        async fn run(&self) -> ToolResult {
            let input = serde_json::json!({ "message": ASK });
            self.in_turn("alice", input.clone(), self.tool.execute(input))
                .await
                .unwrap()
        }

        async fn collect(&self, checkpoint: Option<String>) -> ToolResult {
            let input = serde_json::json!({ "message": ASK });
            self.in_turn(
                "bob",
                input.clone(),
                self.tool.collect(input, self.plan(), checkpoint),
            )
            .await
            .unwrap()
        }

        fn plan(&self) -> serde_json::Value {
            self.plans.pending.lock().unwrap()[0].1.clone()
        }

        fn round(&self) -> String {
            self.plan()["round"].as_str().unwrap().to_string()
        }

        fn checkpoints(&self) -> Vec<String> {
            self.plans.checkpoints.lock().unwrap().clone()
        }

        fn runs(&self) -> Vec<(String, bool, Option<String>)> {
            self.engine.runs()
        }

        fn driver_uuid(&self) -> Option<String> {
            self.sessions
                .get(&self.session_id)
                .unwrap()
                .unwrap()
                .driver_claude_session_uuid(crate::projects::BENCH_PROJECT)
                .map(str::to_string)
        }
    }

    /// A round a runner turn's `coding.session` starts belongs to that
    /// turn's session, which resumes a routed call once its approvers
    /// decide: it keeps routing.
    #[tokio::test]
    async fn nested_coding_session_routed_approval_still_routes() {
        use workforce_agent::tools::approval::testing::{AskEveryCall, RecordingTool};
        let b = bench(false).await;
        let tool = Arc::new(RecordingTool::default());
        b.tool.service.tools.register(tool.clone());
        b.tool.service.set_permission_gate(Arc::new(AskEveryCall));
        let routes = Arc::new(crate::mcp_turn::testing::Routes::default());
        b.tool.service.set_approval_router(routes.clone());
        let sandbox = crate::coding_sandbox::adopt_workdir(
            std::env::temp_dir(),
            vec!["catalogue.*".into()],
            None,
        );
        let turn = workforce_agent::runner::SessionContext {
            key: Some("slack:T1:C1:100.1".into()),
            user_id: Some("alice".into()),
            channel_id: Some("C1".into()),
            ..Default::default()
        };
        let tools = turn
            .scope(async {
                b.tool.round_context(alice().as_deref()).round_tools(
                    sandbox.as_ref(),
                    None,
                    crate::mcp_turn::ClientTimeout::CLAUDE_CODE,
                )
            })
            .await
            .expect("the catalogue is served");
        let (is_error, text) = crate::mcp_turn::testing::text_of(
            &crate::mcp_turn::testing::call(
                tools.endpoint(),
                "catalogue_remove_entry",
                serde_json::json!({ "name": "figma" }),
            )
            .await,
        );
        assert!(!is_error);
        assert_eq!(text, workforce_agent::runner::ROUTED_APPROVAL_TOOL_RESULT);
        let rows = routes.0.lock();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_key, "slack:T1:C1:100.1");
        assert!(tool.runs().is_empty());
    }

    fn alice() -> Option<String> {
        Some("alice".into())
    }

    #[tokio::test]
    async fn a_round_is_journalled_with_its_input_as_recorded_and_checkpoints_its_job() {
        let b = bench(false).await;
        let recorded = serde_json::json!({ "message": "use [[secret:gh]]" });
        let done = b
            .in_turn(
                "alice",
                recorded.clone(),
                b.tool
                    .execute(serde_json::json!({ "message": "use ghp_plain" })),
            )
            .await
            .unwrap();
        assert!(done.success, "{:?}", done.error);
        let (input, plan) = b.plans.pending.lock().unwrap()[0].clone();
        assert_eq!(
            input, recorded,
            "the journal keeps the marker, never the secret"
        );
        assert!(!plan.to_string().contains("ghp_plain"));
        assert_eq!(plan["identity"], "alice");
        assert_eq!(b.checkpoints(), vec![format!("{}.0.0", b.round())]);
    }

    #[tokio::test]
    async fn collecting_bills_the_round_s_own_payer() {
        let b = bench(false).await;
        let input = serde_json::json!({ "message": ASK });
        let heartbeat = TurnPrincipal {
            run_as: Some("heartbeat".into()),
            bill_to: Some("alice".into()),
        };
        let journal: Arc<dyn workforce_agent::turn_journal::TurnJournal> = b.plans.clone();
        let done = workforce_agent::runner::CURRENT_SESSION_ID
            .scope(
                b.session_id.clone(),
                crate::user_config::as_principal(
                    &heartbeat,
                    ToolCall::new(Some(journal), input.clone()).scope(b.tool.execute(input)),
                ),
            )
            .await
            .unwrap();
        assert!(done.success, "{:?}", done.error);
        assert_eq!(b.plan()["bill_to"], "alice");

        let collected = b.collect(b.checkpoints().pop()).await;

        assert!(collected.success, "{:?}", collected.error);
        assert_eq!(
            b.engine.billed(),
            vec![Some("alice".to_string()); 2],
            "a round collected after a restart is still paid by whom it billed"
        );
    }

    #[tokio::test]
    async fn collecting_resumes_at_the_recorded_job_as_the_round_s_own_user() {
        let b = bench(false).await;
        b.run().await;
        let round = b.round();
        let uuid = b.driver_uuid();

        let collected = b.collect(b.checkpoints().pop()).await;
        assert!(collected.success, "{:?}", collected.error);
        assert_eq!(
            Some(collected.output["pr_url"].as_str().unwrap()),
            crate::coding_session_tool::parse_sentinel(ANSWER, "PR_URL:").as_deref()
        );
        assert_eq!(
            b.runs(),
            vec![
                (format!("{round}.0.0"), false, alice()),
                (format!("{round}.0.0"), true, alice()),
            ]
        );
        assert_eq!(b.driver_uuid(), uuid, "collecting prepares nothing again");
        assert_eq!(
            b.plans.pending.lock().unwrap().len(),
            1,
            "and journals nothing new"
        );
    }

    #[tokio::test]
    async fn a_round_that_recorded_no_job_is_driven_from_its_start() {
        let b = bench(false).await;
        b.run().await;
        let round = b.round();
        b.collect(None).await;
        assert_eq!(b.runs()[1], (format!("{round}.0.0"), false, alice()));
    }

    #[tokio::test]
    async fn a_round_collected_after_its_cold_restart_resumes_there_alone() {
        let b = bench(true).await;
        b.run().await;
        let round = b.round();
        let cold = serde_json::from_value::<RoundPlan>(b.plan())
            .unwrap()
            .key
            .cold_uuid()
            .to_string();
        assert_eq!(b.driver_uuid().as_deref(), Some(cold.as_str()));
        assert_eq!(
            b.checkpoints(),
            vec![format!("{round}.0.0"), format!("{round}-cold.0.0")]
        );
        let discarded = b.seats.closed.lock().unwrap().len();

        b.collect(b.checkpoints().pop()).await;
        assert_eq!(
            b.runs()[2..],
            [(format!("{round}-cold.0.0"), true, alice())],
            "the lost leg is not run again"
        );
        assert_eq!(b.driver_uuid().as_deref(), Some(cold.as_str()));
        assert_eq!(
            b.seats.closed.lock().unwrap().len(),
            discarded,
            "and nothing is discarded again"
        );
    }

    #[tokio::test]
    async fn a_round_on_this_host_reports_the_files_it_changed_for_the_compile_gate() {
        let writing = || Rounds {
            writes: vec![("src/lib.rs", "pub fn broken("), ("src/new.rs", "")],
            ..Default::default()
        };
        let here = || Seats {
            local: true,
            repo: true,
            ..Default::default()
        };
        let gated = bench_gated(writing(), here(), ProgramsMode::On).await;
        let ran = gated.run().await;
        assert!(ran.success, "{:?}", ran.error);
        let changed: Vec<PathBuf> = ran.output[CHANGED_FILES]
            .as_array()
            .expect("the files the round changed")
            .iter()
            .map(|path| PathBuf::from(path.as_str().unwrap()))
            .collect();
        assert_eq!(changed.len(), 2, "{changed:?}");
        assert!(changed.iter().all(|path| path.is_absolute()));
        assert!(changed[0].ends_with("src/lib.rs") && changed[1].ends_with("src/new.rs"));
        assert!(std::fs::read_to_string(&changed[0])
            .unwrap()
            .contains("broken"));

        let remote = bench_gated(writing(), Seats::default(), ProgramsMode::On).await;
        let ran = remote.run().await;
        assert!(ran.success);
        assert!(
            ran.output.get(CHANGED_FILES).is_none(),
            "a worker's files are not on this host"
        );

        for gate in [ProgramsMode::OptIn, ProgramsMode::Off] {
            let ungated = bench_gated(writing(), here(), gate).await;
            let ran = ungated.run().await;
            assert!(ran.success);
            assert!(
                ran.output.get(CHANGED_FILES).is_none(),
                "{gate:?}: an agent without the gate is not watched"
            );
        }
    }

    #[tokio::test]
    async fn a_round_on_a_worker_that_does_not_keep_its_jobs_is_not_journalled() {
        let b = bench_on(
            false,
            Seats {
                unkept: true,
                ..Default::default()
            },
        )
        .await;
        assert!(b.run().await.success);
        assert!(
            b.plans.pending.lock().unwrap().is_empty(),
            "nothing a later turn could collect"
        );
    }

    #[tokio::test]
    async fn abandoning_ends_the_recorded_job_in_its_workspace_without_preparing_it() {
        let b = bench(false).await;
        b.run().await;
        let mark = b.checkpoints().pop();
        let plan: RoundPlan = serde_json::from_value(b.plan()).unwrap();
        workforce_agent::runner::CURRENT_SESSION_ID
            .scope(b.session_id.clone(), b.tool.abandon(b.plan(), mark.clone()))
            .await;
        let scope = workspace_scope(false, &b.session_id, &plan.uuid.to_string());
        assert_eq!(
            b.seats.ended.lock().unwrap().clone(),
            vec![(scope, mark.unwrap())]
        );
    }

    #[tokio::test]
    async fn a_checkpoint_of_another_round_is_an_unknown_outcome() {
        let b = bench(false).await;
        b.run().await;
        let collected = b.collect(Some("elsewhere.0.0".into())).await;
        assert_eq!(collected.error.as_deref(), Some(OUTCOME_UNKNOWN));
        assert_eq!(b.runs().len(), 1);
    }

    #[test]
    fn parse_sentinel_extracts_first_token_and_tolerates_markdown() {
        let text = "did the work\nSTATUS: DONE\nBRANCH: docs/roadmap\nPR_URL: https://github.com/o/r/pull/7";
        assert_eq!(parse_sentinel(text, "STATUS:").as_deref(), Some("DONE"));
        assert_eq!(
            parse_sentinel(text, "PR_URL:").as_deref(),
            Some("https://github.com/o/r/pull/7")
        );
        // Trailing prose after the value is dropped (first token only).
        assert_eq!(
            parse_sentinel("STATUS: DONE (instruction handled)", "STATUS:").as_deref(),
            Some("DONE")
        );
        // Markdown decoration is stripped.
        assert_eq!(
            parse_sentinel("**STATUS:** BLOCKED", "STATUS:").as_deref(),
            Some("BLOCKED")
        );
        // Last occurrence wins (Claude Code may restate mid-reply).
        assert_eq!(
            parse_sentinel("PR_URL: old\n...\nPR_URL: https://x/pull/9", "PR_URL:").as_deref(),
            Some("https://x/pull/9")
        );
        assert_eq!(parse_sentinel("no sentinels here", "PR_URL:"), None);
    }

    #[test]
    fn driver_budget_falls_back_to_defaults_when_unset() {
        use workforce_common::settings::{
            CodingSessionConfig, DEFAULT_DRIVER_MAX_TURNS, DEFAULT_DRIVER_TIMEOUT_SECS,
        };

        let unset = DriverBudget::from(&CodingSessionConfig::default());
        assert_eq!(unset.max_turns, DEFAULT_DRIVER_MAX_TURNS);
        assert_eq!(
            unset.wall_clock,
            Duration::from_secs(DEFAULT_DRIVER_TIMEOUT_SECS)
        );

        let configured = DriverBudget::from(&CodingSessionConfig {
            max_turns: 12,
            timeout_secs: 300,
            ..Default::default()
        });
        assert_eq!(configured.max_turns, 12);
        assert_eq!(configured.wall_clock, Duration::from_secs(300));
    }

    fn budget() -> DriverBudget {
        DriverBudget {
            max_turns: 80,
            wall_clock: Duration::from_secs(20 * 60),
        }
    }

    fn prompt_for(workspace: Workspace) -> String {
        driver_append_prompt(budget(), workspace, "", &[])
    }

    #[test]
    fn driver_profile_carries_the_install_defaults_and_the_cache_env() {
        use workforce_common::settings::CodingSessionConfig;

        let unset = DriverProfile::from(&CodingSessionConfig::default());
        assert!(unset.model.is_none() && unset.effort.is_none());
        assert!(unset.preamble.is_empty());
        assert!(unset.cache_env().is_empty());
        assert_eq!(unset.setup_runner(None).timeout, unset.budget.wall_clock);

        let configured = DriverProfile::from(&CodingSessionConfig {
            model: Some("sonnet".into()),
            effort: Some("high".into()),
            preamble: "CI is the verifier.".into(),
            cache_dir: Some("/fast/cache".into()),
            timeout_secs: 300,
            ..Default::default()
        });
        assert_eq!(configured.model.as_deref(), Some("sonnet"));
        assert_eq!(configured.effort.as_deref(), Some("high"));
        assert!(configured
            .cache_env()
            .iter()
            .any(|(k, v)| k == "XDG_DATA_HOME" && v == "/fast/cache/share"));
        let endpoint = RunnerEndpoint::Unix(PathBuf::from("/run/exec.sock"));
        let runner = configured.setup_runner(Some(&endpoint));
        assert_eq!(runner.runner.as_ref(), Some(&endpoint));
        assert!(runner.env.ends_with(&configured.cache_env()));
        assert!(runner.env.iter().any(|(k, _)| k == "PATH"));
        assert_eq!(runner.timeout, Duration::from_secs(300));
    }

    #[test]
    fn build_round_prefers_the_call_over_the_install_defaults() {
        let profile = DriverProfile {
            budget: budget(),
            model: Some("sonnet".into()),
            effort: Some("high".into()),
            preamble: String::new(),
            cache_dir: Some(PathBuf::from("/fast/cache")),
        };
        let build = |input: serde_json::Value| {
            let input: Input = serde_json::from_value(input).unwrap();
            CodingSessionTool::build_round(
                &[],
                Uuid::nil(),
                false,
                &input,
                None,
                &profile,
                Workspace::ProjectRepos,
                None,
            )
        };
        let defaults = build(serde_json::json!({ "message": "go" }));
        assert_eq!(defaults.model.as_deref(), Some("sonnet"));
        assert_eq!(defaults.effort.as_deref(), Some("high"));
        assert_eq!(
            defaults.cache_dir.as_deref(),
            Some(Path::new("/fast/cache"))
        );
        assert_eq!(defaults.timeout, budget().wall_clock);
        assert_eq!(defaults.max_turns, Some(budget().max_turns));
        assert_eq!(
            defaults.continuity,
            Continuity::Session {
                id: Uuid::nil(),
                resume: false
            }
        );
        assert!(defaults.tools.iter().any(|t| t == "Bash"));
        assert!(defaults.memory.claude_md && !defaults.memory.auto_memory);

        let explicit =
            build(serde_json::json!({ "message": "go", "model": "opus", "effort": "max" }));
        assert_eq!(explicit.model.as_deref(), Some("opus"));
        assert_eq!(explicit.effort.as_deref(), Some("max"));
    }

    #[test]
    fn driver_append_prompt_carries_install_notes_setup_and_the_reply_rule() {
        let bare = prompt_for(Workspace::ProjectRepos);
        assert!(bare.contains("never leave a background command running"));
        assert!(bare.contains("what you left to CI or a later round"));
        assert!(!bare.contains("Notes for this install"));
        assert!(!bare.contains("Workspace setup ran"));

        let setup = vec![
            SetupReport {
                alias: "rails".into(),
                command: "bundle install".into(),
                outcome: crate::projects::setup::SetupOutcome::Ok,
            },
            SetupReport {
                alias: "web".into(),
                command: "pnpm install".into(),
                outcome: crate::projects::setup::SetupOutcome::TimedOut,
            },
        ];
        let full = driver_append_prompt(
            budget(),
            Workspace::ProjectRepos,
            "  CI is the verifier; do not run pnpm install.  ",
            &setup,
        );
        assert!(
            full.contains("Notes for this install:\nCI is the verifier; do not run pnpm install.")
        );
        assert!(full.contains("Workspace setup ran before this round:\n- `rails`: `bundle install` — ok\n- `web`: `pnpm install` — timed out"));
        assert!(
            full.find("Workspace setup ran") < full.find("Notes for this install"),
            "the tree's state comes before the install's notes"
        );
        assert!(full.find("Notes for this install") < full.find("BUDGET:"));
    }

    #[test]
    fn driver_append_prompt_states_the_budget_and_keeps_the_sentinel_contract() {
        // Every workspace shares one protocol, so the sentinels the tool parses
        // back out must survive any prompt edit in all of them.
        for workspace in [
            Workspace::ProjectRepos,
            Workspace::Bench { github: true },
            Workspace::Bench { github: false },
        ] {
            let prompt = prompt_for(workspace);
            assert!(prompt.contains("about 20 minutes of wall clock and 80 turns"));
            for sentinel in ["STATUS:", "BRANCH:", "PR_URL:"] {
                assert!(
                    prompt.contains(sentinel),
                    "{workspace:?} prompt must document {sentinel}"
                );
            }
        }
        assert_eq!(
            parse_sentinel("STATUS: DONE", "STATUS:").as_deref(),
            Some("DONE")
        );
    }

    #[test]
    fn driver_append_prompt_describes_the_workspace_it_was_given() {
        let repos = prompt_for(Workspace::ProjectRepos);
        assert!(repos.contains("one subdirectory per repo"));
        assert!(!repos.contains("bench"));

        // An authenticated bench must be told to bring code in itself —
        // nothing is pre-cloned, and that is the whole point of it.
        let bench = prompt_for(Workspace::Bench { github: true });
        assert!(bench.contains("gh repo clone"));
        assert!(bench.contains("persists across rounds"));
        // The sweeper reclaims idle benches, so the driver must not treat one
        // as permanent storage — the /cc card makes the same disclosure.
        assert!(bench.contains("reclaimed once left idle"));
        assert!(
            bench.contains("gh pr create"),
            "a cloned repo still ends in a PR"
        );

        // Without a token, say so instead of letting Claude discover it by
        // failing a clone halfway through a round — and withhold the protocol
        // that assumes a remote, which would contradict the environment.
        let offline = prompt_for(Workspace::Bench { github: false });
        assert!(offline.contains("No GitHub token is configured"));
        assert!(
            offline.contains("reclaimed once left idle"),
            "both bench variants are swept, so both must say so"
        );
        assert!(
            !offline.contains("push anything that matters"),
            "a bench that cannot push must not be told to"
        );
        assert!(!offline.contains("gh repo clone"));
        assert!(!offline.contains("gh pr create"));
        assert!(!offline.contains("pull request"));
    }

    #[test]
    fn workspace_resolve_reads_the_project_and_its_credentials() {
        let mut with_repos = Project::new("allan", "harbor");
        with_repos.repos.push(workforce_projects::ProjectRepo {
            alias: "rails".into(),
            source: workforce_projects::RepoSource::Github {
                owner: "acme".into(),
                repo: "rails".into(),
            },
            default_branch: None,
            setup: None,
        });
        assert_eq!(
            Workspace::resolve(&with_repos, None),
            Workspace::ProjectRepos
        );

        let bench = Project::new("allan", "bench");
        assert_eq!(
            Workspace::resolve(&bench, Some("ghp_x")),
            Workspace::Bench { github: true }
        );
        assert_eq!(
            Workspace::resolve(&bench, None),
            Workspace::Bench { github: false }
        );
    }

    #[test]
    fn input_project_is_optional_so_a_bare_message_reaches_the_bench() {
        let bench: Input =
            serde_json::from_value(serde_json::json!({ "message": "build a wasm skill" })).unwrap();
        assert_eq!(bench.project, None);
        assert!(!bench.new_session);

        let named: Input = serde_json::from_value(
            serde_json::json!({ "project": "harbor", "message": "fix the build" }),
        )
        .unwrap();
        assert_eq!(named.project.as_deref(), Some("harbor"));

        // A message is still the one thing every round needs.
        assert!(serde_json::from_value::<Input>(serde_json::json!({ "project": "x" })).is_err());
    }

    #[test]
    fn rotation_due_on_round_cap_or_idle() {
        let now = Utc::now();
        assert!(
            !rotation_due(0, Some(now), now),
            "fresh, active → no rotation"
        );
        assert!(
            rotation_due(DRIVER_ROTATE_AFTER_ROUNDS, Some(now), now),
            "round cap → rotate"
        );
        let idle = now - chrono::Duration::seconds(DRIVER_ROTATE_AFTER_IDLE_SECS + 1);
        assert!(
            rotation_due(1, Some(idle), now),
            "idle past threshold → rotate"
        );
        assert!(
            !rotation_due(1, Some(now - chrono::Duration::seconds(60)), now),
            "recent activity under cap → no rotation"
        );
        assert!(
            !rotation_due(0, None, now),
            "no prior reply → idle can't trigger"
        );
    }
}
