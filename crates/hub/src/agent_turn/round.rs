//! One round of a CLI turn: a single run of the CLI on the turn's
//! transcript, and what comes in while it runs.

#[cfg(test)]
use crate::subscription::test_support::Scoped as _;
use std::path::PathBuf;
use std::sync::Arc;

use eyre::{eyre, Result};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use workforce_agent::llm::provider::thinking_budget_override;
use workforce_agent::llm::types::{ContentBlock, ImageSource, LlmMessage, Role, TokenUsage};
use workforce_agent::runner::{InjectedMessage, InjectedUserMessage, ProgressEvent};

use super::cli::{raster_extension, CliTurn, Instructions, Placed};
use super::conversation::Transcript;
use super::CliKind;
use crate::claude_code::{
    self, ClaudeCodeConfig, ClaudeCodeResult, Completion, Contained, Interrupted, RemotePlacement,
};
use crate::coding_engine::{CodingRound, Continuity};
use crate::coding_sandbox::CodingSandbox;
use crate::mcp_turn::RoundTools;
use crate::serve::AgentService;
use crate::subscription::completion::{render, split};
use crate::subscription::Login;

/// The CLI's only built-in: the loader for the MCP tools' deferred schemas.
pub const TOOL_SEARCH: &str = "ToolSearch";

/// What a turn is told when the person asks it to stop.
const STOP_REQUEST: &str = "[SYSTEM] The user asked you to stop. Wrap up your current operation, \
summarize what you've done so far, and acknowledge the user's request. Do NOT start any new tool \
calls.";

/// What one round saw besides its result.
pub(super) struct Seen {
    /// The model answered at least once.
    replied: bool,
    /// The text it streamed, for a round cut short.
    pub(super) streamed: String,
}

impl Seen {
    /// Whether the round put anything before the person.
    pub(super) fn produced(&self) -> bool {
        self.replied || !self.streamed.is_empty()
    }
}

/// How one round ended.
pub(super) enum RoundEnd {
    Finished(Box<ClaudeCodeResult>),
    /// The person asked the turn to stop and the round was cut short, having
    /// spent this much.
    Stopped(TokenUsage),
}

/// A round's input: its content blocks, and the round's turn cap.
pub(crate) struct RoundInput {
    blocks: Vec<ContentBlock>,
    max_turns: u32,
}

impl RoundInput {
    /// `blocks`, then the `follow_ups` the round takes, then — when the turn
    /// is `stopping` — the request to wrap up, with one turn to answer in.
    pub(crate) fn of(
        blocks: Vec<ContentBlock>,
        follow_ups: &[InjectedUserMessage],
        stopping: bool,
        max_turns: u32,
    ) -> Self {
        Self {
            blocks: blocks
                .into_iter()
                .chain(follow_up_block(follow_ups))
                .chain(stopping.then(|| ContentBlock::Text {
                    text: STOP_REQUEST.to_string(),
                }))
                .collect(),
            max_turns: if stopping { 1 } else { max_turns },
        }
    }
}

/// One round of a turn: its command and what it runs against.
pub(crate) struct Round<'a> {
    pub(crate) service: &'a AgentService,
    pub(crate) turn: &'a CliTurn,
    pub(crate) model: &'a str,
    pub(crate) login: &'a Login,
    pub(crate) sandbox: &'a Arc<dyn CodingSandbox>,
    pub(crate) instructions: &'a Instructions,
    pub(crate) tools: Option<&'a RoundTools>,
    pub(crate) transcript: &'a Transcript,
    /// The effort the session's thread was opened at (`effort:` on a `/cc`
    /// or `/cx` command), which beats the agent's configured one.
    pub(crate) pinned_effort: Option<&'a str>,
    pub(crate) deadline: Instant,
    #[cfg(test)]
    pub(crate) program: Option<&'a PathBuf>,
}

/// A round ready to run on its CLI.
enum Invocation {
    Claude(Box<ClaudeCodeConfig>),
    Codex {
        round: Box<CodingRound>,
        /// The images its prompt shows, in the workspace for the round.
        images: Option<(Placed, Vec<PathBuf>)>,
    },
}

impl Round<'_> {
    /// Run the round on `input`. Follow-ups arriving meanwhile wait in
    /// `pending` for the next round; a stop cuts the round short.
    pub(super) async fn run(
        &self,
        input: &RoundInput,
        mut inbox: Option<&mut mpsc::Receiver<InjectedMessage>>,
        pending: &mut Vec<InjectedUserMessage>,
    ) -> Result<(RoundEnd, Seen)> {
        let cancel = self.turn.cancel.child_token();
        let (progress, watched) = mpsc::channel(64);
        let watcher = tokio::spawn(watch(watched, self.turn.progress.clone()));
        let token = cancel.clone();
        let run = Box::pin(async move {
            let invocation = self.invocation(input).await?;
            self.invoke(invocation, token, progress).await
        });
        tokio::pin!(run);
        let mut stopped = false;
        let outcome = loop {
            tokio::select! {
                outcome = &mut run => break outcome,
                Some(message) = next(inbox.as_deref_mut()) => match message {
                    InjectedMessage::SteerStop { .. } => {
                        stopped = true;
                        cancel.cancel();
                    }
                    InjectedMessage::UserMessage { text, user, external_id } => {
                        pending.push(InjectedUserMessage { user, text, external_id });
                    }
                    InjectedMessage::ApprovalResponse { .. } => {}
                },
            }
        };
        let seen = watcher.await.unwrap_or(Seen {
            replied: false,
            streamed: String::new(),
        });
        match outcome {
            Ok(result) => Ok((RoundEnd::Finished(Box::new(result)), seen)),
            Err(e) if self.turn.cancel.is_cancelled() => Err(e),
            Err(e) if stopped => Ok((
                RoundEnd::Stopped(
                    e.downcast_ref::<Interrupted>()
                        .map(|interrupted| interrupted.spent.clone())
                        .unwrap_or_default(),
                ),
                seen,
            )),
            Err(e) => Ok((
                RoundEnd::Finished(Box::new(ClaudeCodeResult {
                    text: format!("{e:#}"),
                    completion: Completion::Failed,
                    exit_code: -1,
                    ..ClaudeCodeResult::default()
                })),
                seen,
            )),
        }
    }

    /// What runs the round on `input` on the turn's CLI.
    async fn invocation(&self, input: &RoundInput) -> Result<Invocation> {
        let message = [LlmMessage {
            role: Role::User,
            content: input.blocks.clone(),
        }];
        match self.turn.cli {
            CliKind::ClaudeCode => {
                let prompt = self
                    .instructions
                    .file
                    .as_ref()
                    .ok_or_else(|| eyre!("the system prompt was never placed"))?;
                Ok(Invocation::Claude(Box::new(self.config(
                    input.max_turns,
                    render(self.turn.cli.route(), &message)?,
                    &prompt.path,
                ))))
            }
            CliKind::Codex => self.codex(&message).await,
        }
    }

    /// Run `invocation` until it ends or `cancel` cuts it short.
    async fn invoke(
        &self,
        invocation: Invocation,
        cancel: CancellationToken,
        progress: mpsc::Sender<ProgressEvent>,
    ) -> Result<ClaudeCodeResult> {
        let run = async {
            match invocation {
                Invocation::Claude(config) => {
                    claude_code::run(&config, None, None, Some(cancel), Some(progress)).await
                }
                Invocation::Codex { round, images } => {
                    let (_placed, images) = images.unzip();
                    crate::coding_engine::CodexEngine::drive_contained(
                        self.service.subscriptions(),
                        self.sandbox.as_ref(),
                        &round,
                        self.login,
                        self.tools.cloned(),
                        images.unwrap_or_default(),
                        Some(cancel),
                        Some(progress),
                    )
                    .await
                }
            }
        };
        #[cfg(test)]
        let run = async {
            match self.program {
                Some(program) => self.turn.cli.scoped(program.clone(), run).await,
                None => run.await,
            }
        };
        run.await
    }

    /// The effort the turn's CLI is asked for: the session's pinned choice
    /// when the CLI takes it, then the agent's `[llm] effort` (unless a
    /// voice turn's budget replaces it), then what its thinking budget asks.
    fn effort(&self) -> Option<String> {
        let cli = self.turn.cli;
        self.pinned_effort
            .filter(|pinned| cli.accepts(pinned))
            .map(str::to_ascii_lowercase)
            .or_else(|| {
                workforce_agent::llm::provider::effective_effort()
                    .map(|effort| cli.effort_for(effort).to_string())
            })
            .or_else(|| {
                thinking_budget_override()
                    .or(self.service.effective_llm.thinking_budget_tokens)
                    .and_then(|budget| cli.effort(budget))
                    .map(str::to_string)
            })
    }

    /// The contained Claude Code invocation for one round of the turn, sent
    /// `input` and the system prompt in `prompt_file`, held to `max_turns`.
    pub(crate) fn config(
        &self,
        max_turns: u32,
        input: Vec<Value>,
        prompt_file: &std::path::Path,
    ) -> ClaudeCodeConfig {
        let session_id = self.transcript.slot.as_ref().map(|slot| slot.id);
        ClaudeCodeConfig {
            max_turns: Some(max_turns),
            allowed_tools: self
                .tools
                .map(RoundTools::allowed_tools)
                .unwrap_or_default(),
            effort: self.effort(),
            mcp: self.tools.cloned(),
            session_id,
            resume: self.transcript.resume,
            persist_session: session_id.is_some(),
            remote: self.sandbox.placement().map(|placement| RemotePlacement {
                jobs: false,
                ..placement
            }),
            ..ClaudeCodeConfig::contained(
                self.sandbox.workdir().to_path_buf(),
                self.login.clone(),
                Contained {
                    tools: self
                        .tools
                        .map(|_| vec![TOOL_SEARCH.to_string()])
                        .unwrap_or_default(),
                    system_prompt_file: self.sandbox.workdir().join(prompt_file),
                    input,
                    tool_timeout: self.tools.map(|tools| tools.client_timeout().client),
                    exclude_dynamic: true,
                },
                self.model.to_string(),
                self.deadline.saturating_duration_since(Instant::now()),
            )
        }
    }

    /// The contained Codex round sent `message`'s text with the agent's
    /// prompt, and its images as files in the workspace; it continues the
    /// transcript's thread when there is one. Codex takes no turn cap, and is
    /// shown raster images alone: any other is named to it instead.
    async fn codex(&self, message: &[LlmMessage]) -> Result<Invocation> {
        let (text, images) = split(self.turn.cli.route(), message)?;
        let shown: Vec<(&str, &ImageSource)> = images
            .iter()
            .filter_map(|image| Some((raster_extension(&image.media_type)?, image)))
            .collect();
        let task = images
            .iter()
            .filter(|image| raster_extension(&image.media_type).is_none())
            .fold(text, |task, image| {
                format!(
                    "{task}\n[An attached {} image can't be shown here: only PNG, JPEG, GIF \
                     and WebP images can be.]",
                    image.media_type
                )
            });
        let images = match shown.is_empty() {
            true => None,
            false => Some(Placed::images(self.sandbox, &shown).await?),
        };
        let round = CodingRound {
            model: Some(self.model.to_string()),
            effort: self.effort(),
            system_prompt: Some(self.instructions.text.clone()),
            continuity: self
                .transcript
                .slot
                .as_ref()
                .map_or(Continuity::OneShot, |slot| Continuity::Session {
                    id: slot.id,
                    resume: self.transcript.resume,
                }),
            ..CodingRound::one_shot(
                task,
                self.deadline.saturating_duration_since(Instant::now()),
            )
        };
        Ok(Invocation::Codex {
            round: Box::new(round),
            images,
        })
    }
}

/// Relay a round's progress to the turn's watcher, noting whether the model
/// answered and what it streamed.
async fn watch(
    mut events: mpsc::Receiver<ProgressEvent>,
    progress: Option<mpsc::Sender<ProgressEvent>>,
) -> Seen {
    let mut seen = Seen {
        replied: false,
        streamed: String::new(),
    };
    while let Some(event) = events.recv().await {
        match &event {
            ProgressEvent::LlmCall { .. } => seen.replied = true,
            ProgressEvent::StreamDelta { text } => seen.streamed.push_str(text),
            _ => {}
        }
        if let Some(progress) = &progress {
            let _ = progress.send(event).await;
        }
    }
    seen
}

/// The next message on `inbox`; never, without one.
async fn next(inbox: Option<&mut mpsc::Receiver<InjectedMessage>>) -> Option<InjectedMessage> {
    match inbox {
        Some(inbox) => inbox.recv().await,
        None => std::future::pending().await,
    }
}

/// The follow-ups a round held back and those waiting on `inbox`, in the
/// order they came, and whether a stop is among them.
pub(super) fn drain(
    inbox: Option<&mut mpsc::Receiver<InjectedMessage>>,
    pending: &mut Vec<InjectedUserMessage>,
) -> (Vec<InjectedUserMessage>, bool) {
    let waiting: Vec<InjectedMessage> = inbox
        .map(|inbox| std::iter::from_fn(|| inbox.try_recv().ok()).collect())
        .unwrap_or_default();
    waiting.into_iter().fold(
        (std::mem::take(pending), false),
        |(mut follow_ups, stop), message| match message {
            InjectedMessage::UserMessage {
                text,
                user,
                external_id,
            } => {
                follow_ups.push(InjectedUserMessage {
                    user,
                    text,
                    external_id,
                });
                (follow_ups, stop)
            }
            InjectedMessage::SteerStop { .. } => (follow_ups, true),
            InjectedMessage::ApprovalResponse { .. } => (follow_ups, stop),
        },
    )
}

/// The follow-ups a round takes, as a block of its input.
fn follow_up_block(follow_ups: &[InjectedUserMessage]) -> Option<ContentBlock> {
    (!follow_ups.is_empty()).then(|| ContentBlock::Text {
        text: format!(
            "[FOLLOW-UP from the user — incorporate this into your current task]\n\n{}",
            follow_ups
                .iter()
                .map(|f| f.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        ),
    })
}

/// What a round after `previous` starts from: nothing more than its own
/// messages when it `resumes` the transcript, else the previous input and
/// the answer it drafted.
pub(super) fn continued(resumes: bool, previous: &RoundInput, draft: &str) -> Vec<ContentBlock> {
    match resumes {
        true => Vec::new(),
        false => previous
            .blocks
            .iter()
            .cloned()
            .chain((!draft.is_empty()).then(|| ContentBlock::Text {
                text: format!("[YOUR ANSWER SO FAR]\n{draft}"),
            }))
            .collect(),
    }
}
