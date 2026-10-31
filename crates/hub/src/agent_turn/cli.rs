//! One agent turn on a CLI route: the Claude Code or Codex CLI, contained to
//! the agent's own tools over MCP, answering as the agent on the payer's
//! plan.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use eyre::{ensure, eyre, Result};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workforce_agent::llm::anthropic::CACHE_BREAK_MARKER;
use workforce_agent::llm::types::{ContentBlock, ImageSource, TokenUsage};
use workforce_agent::runner::{
    note_served, scope_turn, InjectedMessage, InjectedUserMessage, ProgressEvent, RunResult,
    StreamDiscardReason, ToolInvocation, SELF_SERVE_APPROVAL_TIMEOUT,
};
use workforce_agent::tools::approval::TurnApprovals;
use workforce_agent::turn_journal::TurnJournal;
use workforce_common::session::EngineSlot;
use workforce_common::subscription::Served;

use super::conversation::{opening, Conversation, Transcript};
use super::round::{continued, drain, Round, RoundEnd, RoundInput};
use super::{model_id, spec, tools_model, CliKind, Oversight, RouteMiss, ANSWER_RESERVE};
use crate::claude_code::{ClaudeCodeResult, Completion};
use crate::cli_session::RotationPolicy;
use crate::code::worktree::WorktreeManager;
use crate::coding_engine::codex;
use crate::coding_sandbox::{
    clean_up_detached, CodingSandbox, EmptyScope, HomeSpec, SandboxCommand, SandboxLauncher,
};
use crate::mcp_turn::{ClientTimeout, McpExposure, RoundTools};
use crate::serve::AgentService;
use crate::subscription::completion::failure_of;
use crate::subscription::pools::{HeldSlot, TurnSlot};
use crate::subscription::{Login, SubscriptionError};
use crate::turn_loop::tools::TurnTools;

/// The project every agent turn's workspace is made under: no repos, so
/// the CLI starts in an empty directory of the turn's own.
const AGENT_TURN_PROJECT: &str = "_agent-turn";

/// How the agent's tools are named to Claude Code, which reaches them over
/// MCP.
const CLAUDE_TOOL_NAMING: &str = "\n\n## Tool names\n\
Your tools are served over MCP by the `workforce` server: a tool these instructions call `a.b` \
is `mcp__workforce__a_b`. Load a tool's schema with ToolSearch before you first call it.";

/// What runs agent turns on CLI routes on this hub: where their workspaces
/// are made, and when a kept transcript is replaced.
pub struct AgentTurnRig {
    /// Where Claude Code turns run.
    launcher: Arc<dyn SandboxLauncher>,
    /// The hub's remote coding worker, where Codex turns alone run.
    codex_worker: Option<Arc<dyn SandboxLauncher>>,
    rotation: RotationPolicy,
    /// How long a turn's keyed home outlives its last turn on a worker.
    home_ttl: Duration,
    #[cfg(test)]
    programs: std::collections::HashMap<CliKind, PathBuf>,
}

impl AgentTurnRig {
    pub fn new(
        launcher: Arc<dyn SandboxLauncher>,
        codex_worker: Option<Arc<dyn SandboxLauncher>>,
        rotation: RotationPolicy,
        home_ttl: Duration,
    ) -> Self {
        Self {
            launcher,
            codex_worker,
            rotation,
            home_ttl,
            #[cfg(test)]
            programs: std::collections::HashMap::new(),
        }
    }

    /// Run the rig's turns on `cli` on the scripted CLI at `program`.
    #[cfg(test)]
    pub(crate) fn with_program(mut self, cli: CliKind, program: PathBuf) -> Self {
        self.programs.insert(cli, program);
        self
    }

    /// Where `cli`'s turns are seated: Codex's only on the remote coding
    /// worker, as its patch tool has no switch.
    fn launcher(&self, cli: CliKind) -> Result<Arc<dyn SandboxLauncher>, String> {
        match cli {
            CliKind::ClaudeCode => Ok(Arc::clone(&self.launcher)),
            CliKind::Codex => self
                .codex_worker
                .clone()
                .ok_or_else(|| codex::WORKER_ONLY.to_string()),
        }
    }
}

/// One turn a CLI route is asked to serve.
pub(crate) struct CliTurn {
    pub cli: CliKind,
    pub model: String,
    pub system_prompt: String,
    pub task: Vec<ContentBlock>,
    pub tools: TurnTools,
    pub max_turns: u32,
    pub principal: String,
    pub oversight: Oversight,
    pub self_serve: workforce_agent::tools::approval::SelfServe,
    pub journal: Option<Arc<dyn TurnJournal>>,
    pub progress: Option<mpsc::Sender<ProgressEvent>>,
    pub cancel: CancellationToken,
    pub timeout: Duration,
}

/// How a CLI route took a turn.
pub(crate) enum CliAttempt {
    Served(CliServed),
    /// It produced nothing and ran no tool, so the next route may take the
    /// turn.
    Unserved(RouteMiss),
}

/// A turn a CLI served.
pub(crate) struct CliServed {
    pub text: String,
    /// What this turn's rounds used: each round's own, not its transcript's.
    pub usage: TokenUsage,
    pub rounds: u32,
    pub invocations: Vec<ToolInvocation>,
    pub duration: Duration,
    /// The cost ledger's key for the model that served.
    pub model: String,
    pub injected: Vec<InjectedUserMessage>,
    pub capped: bool,
    /// The transcript the turn continued, when its session keeps one.
    pub slot: Option<EngineSlot>,
    /// The plan that paid for it.
    pub served: Served,
}

impl CliServed {
    /// The turn as a runner's result, the calls `seeded` before it first.
    pub(crate) fn into_result(self, seeded: Vec<ToolInvocation>) -> RunResult {
        RunResult {
            response: self.text,
            total_usage: self.usage,
            rounds: self.rounds,
            tool_invocations: seeded.into_iter().chain(self.invocations).collect(),
            round_details: Vec::new(),
            duration_ms: self.duration.as_millis() as u64,
            model: self.model,
            model_switches: Vec::new(),
            budget_capped: self.capped,
            hit_round_cap: self.capped,
            injected_user_messages: self.injected,
            gate_notice: None,
        }
    }
}

/// What a turn has so far across its rounds.
#[derive(Default)]
struct Tally {
    usage: TokenUsage,
    rounds: u32,
    injected: Vec<InjectedUserMessage>,
}

impl Tally {
    fn spend(&mut self, usage: TokenUsage) {
        self.usage = std::mem::take(&mut self.usage) + usage;
    }

    fn add(&mut self, result: &ClaudeCodeResult) {
        self.spend(result.usage());
        self.rounds += result.num_turns;
    }
}

/// The agent's prompt as a turn's CLI is given it: one text, told how the
/// CLI names its tools.
#[derive(Default)]
pub(super) struct Instructions {
    pub(super) text: String,
    /// Where in the workspace it was placed, for a CLI that reads it there.
    pub(super) file: Option<Placed>,
}

impl Instructions {
    /// The agent's prompt for `cli` in `sandbox`: Claude Code reads it from a
    /// file in the workspace, and Codex is handed it with each round.
    async fn of(cli: CliKind, sandbox: &Arc<dyn CodingSandbox>, stable: &str) -> Result<Self> {
        let named = |note: &str| format!("{}{note}", stable.replace(CACHE_BREAK_MARKER, "\n\n"));
        Ok(match cli {
            CliKind::ClaudeCode => {
                let text = named(CLAUDE_TOOL_NAMING);
                Self {
                    file: Some(Placed::prompt(Arc::clone(sandbox), text.clone()).await?),
                    text,
                }
            }
            CliKind::Codex => Self {
                text: named(&codex::tool_naming()),
                file: None,
            },
        })
    }
}

/// Something a turn put in its workspace, readable by no one but the user
/// its CLI runs as, and removed however the turn ends.
pub(super) struct Placed {
    sandbox: Arc<dyn CodingSandbox>,
    /// Where, relative to the workspace.
    pub(super) path: PathBuf,
}

impl Placed {
    /// Write `prompt` into `sandbox`: on this host by the hub, for the CLI's
    /// user to read; in a remote scope by the scope's own user, the one its
    /// CLI runs as.
    async fn prompt(sandbox: Arc<dyn CodingSandbox>, prompt: String) -> Result<Self> {
        let file = Self {
            path: PathBuf::from(format!(".workforce-system-prompt-{}.md", Uuid::new_v4())),
            sandbox,
        };
        match file.sandbox.placement() {
            None => write_here(&file.here(), prompt.as_bytes()).await?,
            Some(_) => {
                file.in_scope("umask 077 && cat > \"$1\"", &[], prompt)
                    .await?
            }
        }
        Ok(file)
    }

    /// Write `images`, each under its file extension, into a directory of
    /// their own in `sandbox`, as a prompt is written; returns it and each
    /// image's path.
    pub(super) async fn images(
        sandbox: &Arc<dyn CodingSandbox>,
        images: &[(&str, &ImageSource)],
    ) -> Result<(Self, Vec<PathBuf>)> {
        use base64::Engine;
        let dir = Self {
            path: PathBuf::from(format!(".workforce-images-{}", Uuid::new_v4())),
            sandbox: Arc::clone(sandbox),
        };
        let named: Vec<(String, &ImageSource)> = images
            .iter()
            .enumerate()
            .map(|(n, (extension, image))| (format!("{}.{extension}", n + 1), *image))
            .collect();
        let here = dir.here();
        let placed = &dir;
        match sandbox.placement() {
            None => {
                make_dir_here(&here).await?;
                futures::future::try_join_all(named.iter().map(|(name, image)| {
                    let path = here.join(name);
                    async move {
                        let bytes =
                            base64::engine::general_purpose::STANDARD.decode(&image.data)?;
                        write_here(&path, &bytes).await
                    }
                }))
                .await?;
            }
            Some(_) => {
                futures::future::try_join_all(named.iter().map(|(name, image)| async move {
                    placed
                        .in_scope(
                            "umask 077 && mkdir -p \"$1\" && base64 -d > \"$1/$2\"",
                            &[name],
                            image.data.clone(),
                        )
                        .await
                }))
                .await?;
            }
        }
        let paths = named.into_iter().map(|(name, _)| here.join(name)).collect();
        Ok((dir, paths))
    }

    fn here(&self) -> PathBuf {
        self.sandbox.workdir().join(&self.path)
    }

    /// Run `script` in the workspace as the user its CLI runs as, `$1` naming
    /// this path and `args` following it, fed `stdin`.
    async fn in_scope(&self, script: &str, args: &[&str], stdin: String) -> Result<()> {
        let written = self
            .sandbox
            .exec(
                SandboxCommand::new(
                    "sh",
                    ["-c".to_string(), script.to_string(), "sh".to_string()]
                        .into_iter()
                        .chain(std::iter::once(self.path.display().to_string()))
                        .chain(args.iter().map(|arg| arg.to_string())),
                )
                .stdin(stdin),
            )
            .await?;
        ensure!(
            written.success(),
            "exit {}: {}",
            written.exit_code(),
            written.stderr.trim()
        );
        Ok(())
    }
}

/// Whether the CLI on this host runs as the privilege-separated runner,
/// which reads what the hub writes through the group they share.
fn shared_with_runner() -> bool {
    crate::claude_code::sandbox_runner().is_some()
}

/// Write `content` to a new file at `path`: owner-only, or readable by the
/// runner's group when the CLI runs as it.
async fn write_here(path: &Path, content: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .await?;
    file.write_all(content).await?;
    file.flush().await?;
    if shared_with_runner() {
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o640)).await?;
    }
    Ok(())
}

/// Make a new directory at `path`, as private as [`write_here`]'s files.
async fn make_dir_here(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .await?;
    if shared_with_runner() {
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o750)).await?;
    }
    Ok(())
}

/// The file extension of an image a CLI can be shown, by its media type:
/// raster images alone.
pub(super) fn raster_extension(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

impl Drop for Placed {
    /// Removed here when the hub may; else, and in a remote scope, by the
    /// user who wrote it, through the workspace.
    fn drop(&mut self) {
        if self.sandbox.placement().is_none() {
            let here = self.here();
            let removed = match here.is_dir() {
                true => std::fs::remove_dir_all(&here),
                false => std::fs::remove_file(&here),
            };
            match removed {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => {}
            }
        }
        let sandbox = Arc::clone(&self.sandbox);
        let removed =
            SandboxCommand::new("rm", ["-rf".to_string(), self.path.display().to_string()]);
        clean_up_detached(
            format!("a turn's file {} was left behind", self.path.display()),
            async move { sandbox.exec(removed).await.map(drop) },
        );
    }
}

impl AgentTurnRig {
    /// Serve `turn` on its CLI, in `conversation` when it has one. The
    /// turn's injected messages arrive on `inbox`: approval answers settle
    /// its prompts, a follow-up runs another round, a stop cuts the round
    /// short and has the turn wrap up.
    pub(crate) async fn run(
        &self,
        service: &AgentService,
        turn: CliTurn,
        conversation: Option<&Conversation<'_>>,
        inbox: Option<mpsc::Receiver<InjectedMessage>>,
    ) -> Result<CliAttempt> {
        let route = spec(turn.cli, &turn.model);
        let unserved = |reason: String| CliAttempt::Unserved(RouteMiss::new(route.clone(), reason));
        let login = match service.logins.resolve(turn.cli.login()) {
            Ok(login) => login,
            Err(no_login) => return Ok(unserved(SubscriptionError::NoLogin(no_login).to_string())),
        };
        let launcher = match self.launcher(turn.cli) {
            Ok(launcher) => launcher,
            Err(reason) => return Ok(unserved(reason)),
        };
        let started = Instant::now();
        let deadline = started + turn.timeout;
        let slot = match tokio::time::timeout_at(
            deadline,
            TurnSlot::acquire(
                service.subscriptions().pools().turns(),
                &turn.cancel,
                waiting(turn.cli, turn.progress.as_ref(), "waiting for a free slot"),
            ),
        )
        .await
        {
            Ok(Ok(slot)) => slot,
            Ok(Err(_)) => return Err(eyre!("turn cancelled")),
            Err(_) => {
                return Ok(unserved(
                    "no CLI turn slot came free before the turn's deadline".to_string(),
                ))
            }
        };
        let scope = format!(
            "at-{}",
            WorktreeManager::scope_id_for_session(
                conversation
                    .as_ref()
                    .map_or("tool-loop", |c| c.session.id.as_str())
            )
        );
        let sandbox = match (EmptyScope {
            agent_id: service.agent_id.as_str(),
            project: AGENT_TURN_PROJECT,
            scope: &scope,
            home: HomeSpec::Keyed {
                key: scope.clone(),
                ttl_secs: self.home_ttl.as_secs(),
            },
        })
        .seat(
            launcher.as_ref(),
            deadline.into_std(),
            Some(&turn.cancel),
            || {
                waiting(
                    turn.cli,
                    turn.progress.as_ref(),
                    "waiting for the coding worker",
                )
            },
        )
        .await
        {
            Ok(sandbox) => sandbox,
            Err(e) => return Ok(unserved(format!("no workspace for the turn: {e:#}"))),
        };
        let instructions = match Instructions::of(turn.cli, &sandbox, &turn.system_prompt).await {
            Ok(instructions) => instructions,
            Err(e) => {
                return Ok(unserved(format!(
                    "the system prompt could not be written: {e:#}"
                )))
            }
        };
        Box::pin(self.serve(
            service,
            &turn,
            &login,
            Seated {
                sandbox: &sandbox,
                instructions: &instructions,
                slot: &slot,
                started,
            },
            conversation,
            inbox,
        ))
        .await
    }

    /// The rounds of a turn seated in its workspace, until one answers. A
    /// turn that produced nothing and ran no tool is the route's miss;
    /// anything after output is final.
    async fn serve(
        &self,
        service: &AgentService,
        turn: &CliTurn,
        login: &Login,
        seated: Seated<'_>,
        conversation: Option<&Conversation<'_>>,
        inbox: Option<mpsc::Receiver<InjectedMessage>>,
    ) -> Result<CliAttempt> {
        let Seated {
            sandbox,
            instructions,
            slot,
            started,
        } = seated;
        let route = spec(turn.cli, &turn.model);
        let model = model_id(turn.cli, &turn.model, service);
        let deadline = started + turn.timeout;
        let (ledger, mut inbox) = TurnApprovals::split(inbox, turn.self_serve);
        let _ledger_end = ledger.clone().map(TurnApprovals::end_on_drop);
        let tools = scope_turn(turn.progress.clone(), ledger, async {
            RoundTools::bind(
                service,
                &turn.principal,
                McpExposure::Turn(turn.tools.clone()),
                Some(&turn.cancel),
                client_timeout(turn.timeout),
            )
        })
        .await
        .map(|tools| {
            let tools = match turn.oversight {
                Oversight::Attended(resume) => tools.resumed(resume),
                Oversight::Unattended => tools.unattended(),
            };
            let tools = match &turn.journal {
                Some(journal) => tools.with_journal(Arc::clone(journal)),
                None => tools,
            };
            tools
                .for_agent_turn(slot.slot().clone())
                .with_model(tools_model(turn.cli, &model, service))
        });
        if tools.is_none() {
            tracing::warn!(
                agent = %service.agent_id,
                route = %route,
                offered = turn.tools.registry.names().len(),
                "none of the turn's tools can be served over MCP; it runs without tools"
            );
        }

        let mut transcript =
            Transcript::of(conversation, turn.cli, &self.rotation, sandbox.fresh());
        let mut tally = Tally::default();
        let mut pending = Vec::new();
        let (follow_ups, mut stopping) = drain(inbox.as_mut(), &mut pending);
        tally.injected.extend(follow_ups);
        let opening_of = |transcript: &Transcript, tally: &Tally, stopping: bool| {
            RoundInput::of(
                opening(&turn.task, transcript, conversation),
                &tally.injected,
                stopping,
                turn.max_turns,
            )
        };
        let mut input = opening_of(&transcript, &tally, stopping);
        let mut recovered = false;
        let mut produced = false;
        let answer = loop {
            let round = Round {
                service,
                turn,
                model: &model,
                login,
                sandbox,
                instructions,
                tools: tools.as_ref(),
                transcript: &transcript,
                pinned_effort: conversation.and_then(|c| c.session.claude_chat_effort()),
                deadline,
                #[cfg(test)]
                program: self.programs.get(&turn.cli),
            };
            let (end, seen) = Box::pin(round.run(&input, inbox.as_mut(), &mut pending)).await?;
            produced |= seen.produced();
            let result = match end {
                RoundEnd::Stopped(spent) => {
                    tally.spend(spent);
                    produced = true;
                    transcript.resume |= transcript.keeps();
                    input = RoundInput::of(
                        continued(transcript.resume, &input, &seen.streamed),
                        &[],
                        true,
                        1,
                    );
                    stopping = true;
                    continue;
                }
                RoundEnd::Finished(result) => *result,
            };
            if !result.completion.is_usable() {
                if result.completion == Completion::SessionNotFound && !recovered {
                    recovered = true;
                    tracing::warn!(agent = %service.agent_id, route = %route, "the turn's transcript is gone; starting a fresh one");
                    transcript = Transcript::fresh();
                    input = opening_of(&transcript, &tally, stopping);
                    continue;
                }
                let failure =
                    failure_of(login, service.agent_id.as_str(), turn.cli.route(), &result);
                let called = tools.as_ref().is_some_and(|t| !t.invocations().is_empty());
                if !produced && !called && tally.rounds == 0 {
                    return Ok(CliAttempt::Unserved(RouteMiss::new(
                        route,
                        failure.to_string(),
                    )));
                }
                return Err(failure);
            }
            // What a served round says of its login too: how full the plan's
            // windows are, kept as its reading.
            login.settle(&result.round_end(), service.agent_id.as_str());
            tally.add(&result);
            transcript.resume |= transcript.keeps();
            let (follow_ups, stop) = drain(inbox.as_mut(), &mut pending);
            if stopping || (follow_ups.is_empty() && !stop) {
                break result;
            }
            if let Some(progress) = &turn.progress {
                let _ = progress
                    .send(ProgressEvent::StreamDiscard {
                        reason: StreamDiscardReason::Superseded,
                    })
                    .await;
            }
            stopping |= stop;
            tally.injected.extend(follow_ups.iter().cloned());
            input = RoundInput::of(
                continued(transcript.resume, &input, &result.text),
                &follow_ups,
                stopping,
                turn.max_turns,
            );
        };

        let served = login.served(turn.cli.route(), service.agent_id.as_str());
        let slot = transcript.slot.map(|slot| EngineSlot {
            turns: slot.turns + 1,
            last_reply_at: Some(Utc::now()),
            ..slot
        });
        let served_turn = CliServed {
            text: answer.text.clone(),
            usage: tally.usage,
            rounds: tally.rounds.max(1),
            invocations: tools
                .as_ref()
                .map(RoundTools::invocations)
                .unwrap_or_default(),
            duration: started.elapsed(),
            model: served.ledger_model(&model),
            injected: tally.injected,
            capped: answer.completion == Completion::TurnCap,
            slot,
            served: served.clone(),
        };
        note_served(served);
        Ok(CliAttempt::Served(served_turn))
    }
}

/// Where a turn's rounds run: its workspace, how its CLI is given the
/// agent's prompt there, its pool permit, and when it began.
struct Seated<'a> {
    sandbox: &'a Arc<dyn CodingSandbox>,
    instructions: &'a Instructions,
    slot: &'a HeldSlot,
    started: Instant,
}

/// How long a turn's CLI waits on one MCP call, and how much of that the
/// tool may take: all of the turn but the time kept to answer in, which a
/// short turn shrinks to a fifth of itself.
pub(crate) fn client_timeout(timeout: Duration) -> ClientTimeout {
    let client = timeout.saturating_sub(ANSWER_RESERVE.min(timeout / 5));
    ClientTimeout {
        tool: client.saturating_sub(SELF_SERVE_APPROVAL_TIMEOUT),
        client,
    }
}

/// Tell the turn's watcher what its `cli` is waiting for, when anyone
/// watches it.
async fn waiting(cli: CliKind, progress: Option<&mpsc::Sender<ProgressEvent>>, detail: &str) {
    if let Some(progress) = progress {
        let _ = progress
            .send(ProgressEvent::SubActivity {
                source: cli.route().label().to_string(),
                detail: detail.to_string(),
            })
            .await;
    }
}
