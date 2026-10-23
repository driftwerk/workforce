//! One-shot `claude-code` and `codex` completions: a tool-less call answered
//! by the Claude Code CLI on the payer's Claude login, or by the Codex CLI on
//! their ChatGPT login in an empty scope on the coding worker, each run
//! contained so it carries nothing but its prompt. The chain is the retry: a
//! failure names why and moves the chain on.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;
use workforce_agent::llm::image_cap;
use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::llm::streaming::StreamEvent;
use workforce_agent::llm::types::{
    ContentBlock, ImageSource, LlmMessage, LlmResponse, LlmToolDef, Role, StopReason, TokenUsage,
};
use workforce_agent::runner::note_served;
use workforce_common::subscription::{Served, SubscriptionRoute};

use super::llm::{PlanLogins, SubscriptionError};
use super::resolve::Login;
use super::CliKind;
use crate::claude_code::{self, ClaudeCodeConfig, ClaudeCodeResult, Completion, Contained};
use crate::coding_engine::codex;
use crate::coding_engine::{CodexEngine, CodingRound};
use crate::coding_sandbox::{clean_up_detached, EmptyScope, HomeSpec, SandboxLauncher};

/// The longest one completion runs, its wait for a slot included. Under a
/// chain that caps the call it ends sooner, at [`budget`], so it names its
/// own failure before the cap cuts it off.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(240);
/// How often a streaming caller hears the call is alive until its text
/// arrives: the CLI answers only at the end, past the first-token window.
const KEEP_ALIVE: Duration = Duration::from_secs(30);
/// How much longer than [`COMPLETION_TIMEOUT`] a completion's directory may
/// outlive it: its process is killed and drained, then the directory removed.
const LEFTOVER_AFTER: Duration = Duration::from_secs(300);
const SYSTEM_PROMPT_FILE: &str = "system.md";
/// The project a Codex completion's scope is made under: no repos, so the
/// CLI starts in an empty directory.
const CODEX_PROJECT: &str = "_cli-completion";
/// The longest a Codex completion waits for a seat on the worker.
const SEAT_WAIT: Duration = Duration::from_secs(30);

/// The context a completion on `cli` holds.
const fn context_window(cli: CliKind) -> u32 {
    match cli {
        CliKind::ClaudeCode => 200_000,
        CliKind::Codex => 272_000,
    }
}

/// The model a completion on `cli` runs when none is named.
const fn default_model(cli: CliKind) -> &'static str {
    match cli {
        CliKind::ClaudeCode => "sonnet",
        CliKind::Codex => "gpt-6.1-sol",
    }
}

/// A `claude-code` or `codex` provider for tool-less calls, resolving the
/// payer's login per call. Built by [`PlanLogins::provider`].
pub struct CliCompletion {
    plans: PlanLogins,
    cli: CliKind,
    model: Option<String>,
    thinking_budget: Option<u32>,
}

/// A served completion.
struct Answer {
    text: String,
    usage: TokenUsage,
    served: Served,
}

impl CliCompletion {
    pub(crate) fn new(
        plans: PlanLogins,
        cli: CliKind,
        model: Option<&str>,
        thinking_budget: Option<u32>,
    ) -> Self {
        Self {
            plans,
            cli,
            model: model.map(str::to_string),
            thinking_budget,
        }
    }

    /// The effort the completion's thinking budget asks of its CLI.
    fn effort(&self) -> Option<String> {
        self.thinking_budget
            .and_then(|budget| self.cli.effort(budget))
            .map(str::to_string)
    }

    fn model<'a>(&'a self, requested: Option<&'a str>) -> &'a str {
        requested
            .or(self.model.as_deref())
            .unwrap_or(default_model(self.cli))
    }

    async fn complete(
        &self,
        system: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: &str,
        tx: Option<&mpsc::Sender<StreamEvent>>,
    ) -> Result<Answer> {
        let route = self.cli.route();
        if !tools.is_empty() {
            return Err(SubscriptionError::Uncarried(format!(
                "{} serves tool-less completions; tool loops on this route run as the agent's CLI turn",
                route.label()
            ))
            .into());
        }
        let prompt = match self.cli {
            CliKind::ClaudeCode => Prompt::Blocks(render(route, messages)?),
            CliKind::Codex => Prompt::Text(text_only(route, messages)?),
        };
        let login = self
            .plans
            .scope()
            .resolve(route.kind())
            .map_err(SubscriptionError::NoLogin)?;
        let budget = budget(self.plans.scope().request_cap(model));
        let deadline = Instant::now() + budget;
        let system = system.unwrap_or_default();
        let run = keep_alive(tx, async {
            let _permit = tokio::time::timeout_at(
                deadline,
                self.plans
                    .subscriptions()
                    .shared()
                    .pools()
                    .completions()
                    .acquire(None),
            )
            .await
            .map_err(|_| SubscriptionError::Busy {
                route,
                waited: budget,
            })??;
            match prompt {
                Prompt::Blocks(input) => {
                    self.run_claude(&login, system, input, model, deadline)
                        .await
                }
                Prompt::Text(task) => self.run_codex(&login, system, task, model, deadline).await,
            }
            .map_err(|e| eyre!("{} completion failed: {e}", route.label()))
        })
        .await?;
        let result = self.served(&login, run)?;
        Ok(Answer {
            usage: result.usage(),
            text: result.text,
            served: login.served(route, self.plans.scope().caller()),
        })
    }

    /// A contained one-turn `claude` round in a directory of its own.
    async fn run_claude(
        &self,
        login: &Login,
        system: &str,
        input: Vec<Value>,
        model: &str,
        deadline: Instant,
    ) -> Result<ClaudeCodeResult> {
        let workdir = Workdir::create(
            self.plans.subscriptions().shared().completions_dir(),
            system,
        )?;
        let timeout = deadline.saturating_duration_since(Instant::now());
        claude_code::run(
            &self.config(login, &workdir, input, model, timeout),
            None,
            None,
            None,
            None,
        )
        .await
    }

    /// A contained `codex exec` in an empty scope of its own on the coding
    /// worker, closed however the call ends.
    async fn run_codex(
        &self,
        login: &Login,
        system: &str,
        task: String,
        model: &str,
        deadline: Instant,
    ) -> Result<ClaudeCodeResult> {
        let subscriptions = self.plans.subscriptions();
        let launcher = subscriptions
            .codex_worker()
            .ok_or_else(|| eyre!(codex::WORKER_ONLY))?;
        let caller = self.plans.scope().caller();
        let scope = Scope {
            launcher: Arc::clone(launcher),
            agent_id: caller.to_string(),
            scope: format!("cc-{}", uuid::Uuid::new_v4().simple()),
        };
        let seat_by = deadline.min(Instant::now() + SEAT_WAIT);
        let sandbox = EmptyScope {
            agent_id: caller,
            project: CODEX_PROJECT,
            scope: &scope.scope,
            home: HomeSpec::Ephemeral,
        }
        .seat(launcher.as_ref(), seat_by.into_std(), None, || {
            std::future::ready(())
        })
        .await?;
        let round = CodingRound {
            model: Some(model.to_string()),
            effort: self.effort(),
            system_prompt: Some(system.to_string()),
            ..CodingRound::one_shot(task, deadline.saturating_duration_since(Instant::now()))
        };
        CodexEngine::drive_contained(
            subscriptions.shared(),
            sandbox.as_ref(),
            &round,
            login,
            None,
            Vec::new(),
            None,
            None,
        )
        .await
    }

    /// The contained one-turn round a completion runs, in `workdir`, for at
    /// most `timeout`.
    fn config(
        &self,
        login: &Login,
        workdir: &Workdir,
        input: Vec<Value>,
        model: &str,
        timeout: Duration,
    ) -> ClaudeCodeConfig {
        ClaudeCodeConfig {
            max_turns: Some(1),
            effort: self.effort(),
            ..ClaudeCodeConfig::contained(
                workdir.path().to_path_buf(),
                login.clone(),
                Contained {
                    tools: Vec::new(),
                    system_prompt_file: workdir.system_prompt(),
                    input,
                    tool_timeout: None,
                    exclude_dynamic: false,
                },
                model.to_string(),
                timeout,
            )
        }
    }

    /// `result` when the round answered; else why not, after cooling a login
    /// at its limit or marking one refused for good.
    fn served(&self, login: &Login, result: ClaudeCodeResult) -> Result<ClaudeCodeResult> {
        match result.completion {
            Completion::Success => Ok(result),
            _ => Err(failure_of(
                login,
                self.plans.scope().caller(),
                self.cli.route(),
                &result,
            )),
        }
    }
}

/// Why a round on `login` for `caller` did not succeed: what it says about
/// the login, as [`Login::settle`] keeps it, else the CLI's last word.
pub(crate) fn failure_of(
    login: &Login,
    caller: &str,
    route: SubscriptionRoute,
    result: &ClaudeCodeResult,
) -> eyre::Report {
    login.settle(&result.round_end(), caller).map_or_else(
        || eyre!("{} round failed: {}", route.label(), result.failure_line()),
        Into::into,
    )
}

#[async_trait]
impl LlmProvider for CliCompletion {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        _max_tokens: Option<u32>,
        _enable_cache: bool,
    ) -> Result<LlmResponse> {
        let model = self.model(model);
        let answer = self
            .complete(system_prompt, messages, tools, model, None)
            .await?;
        note_served(answer.served);
        Ok(LlmResponse {
            content: vec![ContentBlock::Text { text: answer.text }],
            stop_reason: StopReason::EndTurn,
            usage: answer.usage,
            model: model.to_string(),
        })
    }

    async fn create_message_stream(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        _max_tokens: Option<u32>,
        _enable_cache: bool,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<()> {
        let model = self.model(model);
        let answer = self
            .complete(system_prompt, messages, tools, model, Some(&tx))
            .await?;
        for event in [
            StreamEvent::ContentStart { index: 0 },
            StreamEvent::TextDelta {
                index: 0,
                text: answer.text,
            },
            StreamEvent::ContentStop { index: 0 },
            StreamEvent::MessageDone {
                stop_reason: StopReason::EndTurn,
                usage: answer.usage,
            },
        ] {
            if tx.send(event).await.is_err() {
                break;
            }
        }
        note_served(answer.served);
        Ok(())
    }

    async fn context_floor(&self, _model: &str) -> Option<u32> {
        Some(context_window(self.cli))
    }

    /// Always passes: a login is resolved per call, for a payer that only
    /// exists once a call is made.
    async fn verify_credentials(&self) -> Result<()> {
        Ok(())
    }
}

/// How long a completion may take under a chain that caps the call at `cap`:
/// a fifth less, so it ends first, and never past [`COMPLETION_TIMEOUT`].
fn budget(cap: Option<Duration>) -> Duration {
    cap.map_or(COMPLETION_TIMEOUT, |cap| {
        (cap * 4 / 5).min(COMPLETION_TIMEOUT)
    })
}

/// `work`, telling a streaming caller every [`KEEP_ALIVE`] that it is still
/// under way, so the first-token watchdog waits for it.
async fn keep_alive<T>(
    tx: Option<&mpsc::Sender<StreamEvent>>,
    work: impl std::future::Future<Output = T>,
) -> T {
    let Some(tx) = tx else {
        return work.await;
    };
    tokio::pin!(work);
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + KEEP_ALIVE, KEEP_ALIVE);
    loop {
        tokio::select! {
            done = &mut work => return done,
            _ = ticks.tick() => {
                let _ = tx.try_send(StreamEvent::KeepAlive {
                    within_secs: KEEP_ALIVE.as_secs(),
                });
            }
        }
    }
}

/// A call's messages as the content of the one user message a completion
/// sends: a lone user message as it stands, a longer history as its turns
/// in order, with images kept where they were. Tool calls and results become
/// text and thinking is dropped; a document can't be carried. `claude` sends
/// the images on to the Messages API, so they keep within its limit.
pub(crate) fn render(
    route: SubscriptionRoute,
    messages: &[LlmMessage],
) -> Result<Vec<Value>, SubscriptionError> {
    let messages = match route {
        SubscriptionRoute::ClaudeCode => {
            image_cap::cap_images(Cow::Borrowed(messages), image_cap::ANTHROPIC_MAX_IMAGES)
        }
        _ => Cow::Borrowed(messages),
    };
    let pieces = match &*messages {
        [only] if only.role == Role::User => pieces(route, &only.content),
        history => history
            .iter()
            .map(|message| {
                let role = match message.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                let body = pieces(route, &message.content)?;
                Ok(
                    std::iter::once(Piece::Text(format!("<turn role=\"{role}\">")))
                        .chain(body)
                        .chain([Piece::Text("</turn>".to_string())])
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Result<Vec<_>, SubscriptionError>>()
            .map(|turns| turns.into_iter().flatten().collect()),
    }?;
    Ok(pieces
        .into_iter()
        .fold(Vec::<Piece>::new(), |mut blocks, piece| {
            match (blocks.last_mut(), piece) {
                (Some(Piece::Text(text)), Piece::Text(next)) => {
                    text.push('\n');
                    text.push_str(&next);
                }
                (_, piece) => blocks.push(piece),
            }
            blocks
        })
        .into_iter()
        .map(Piece::into_block)
        .collect())
}

/// A piece of the rendered prompt.
enum Piece {
    Text(String),
    Image(Value),
}

impl Piece {
    fn into_block(self) -> Value {
        match self {
            Self::Text(text) => serde_json::json!({ "type": "text", "text": text }),
            Self::Image(image) => image,
        }
    }
}

fn pieces(
    route: SubscriptionRoute,
    content: &[ContentBlock],
) -> Result<Vec<Piece>, SubscriptionError> {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if text.is_empty() => None,
            ContentBlock::Text { text } => Some(Ok(Piece::Text(text.clone()))),
            ContentBlock::Thinking { .. } => None,
            ContentBlock::Image { .. } => Some(
                serde_json::to_value(block)
                    .map(Piece::Image)
                    .map_err(|e| SubscriptionError::Uncarried(format!("an image: {e}"))),
            ),
            ContentBlock::Document { .. } => Some(Err(SubscriptionError::Uncarried(format!(
                "{} completions carry no documents",
                route.label()
            )))),
            ContentBlock::ToolUse { name, input, .. } => {
                Some(Ok(Piece::Text(format!("[tool call {name}: {input}]"))))
            }
            ContentBlock::ToolResult { content, .. } => {
                Some(Ok(Piece::Text(format!("[tool result: {content}]"))))
            }
        })
        .collect()
}

/// What a completion is asked: content blocks for `claude`'s stream-json
/// input, plain text for `codex`.
enum Prompt {
    Blocks(Vec<Value>),
    Text(String),
}

/// `messages` rendered as the text a `route` that reads only text is sent,
/// and the images among them, which it is shown apart from the text.
pub(crate) fn split(
    route: SubscriptionRoute,
    messages: &[LlmMessage],
) -> Result<(String, Vec<ImageSource>), SubscriptionError> {
    let images = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Image { source } => Some(source.clone()),
            _ => None,
        })
        .collect();
    let texts: Vec<LlmMessage> = messages
        .iter()
        .map(|message| LlmMessage {
            role: message.role.clone(),
            content: message
                .content
                .iter()
                .filter(|block| !matches!(block, ContentBlock::Image { .. }))
                .cloned()
                .collect(),
        })
        .collect();
    let text = render(route, &texts)?
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Ok((text, images))
}

/// `messages` as the text a `route` that reads only text is sent: an image
/// can't be carried.
fn text_only(
    route: SubscriptionRoute,
    messages: &[LlmMessage],
) -> Result<String, SubscriptionError> {
    match split(route, messages)? {
        (text, images) if images.is_empty() => Ok(text),
        _ => Err(SubscriptionError::Uncarried(format!(
            "{} completions carry no images",
            route.label()
        ))),
    }
}

/// A Codex completion's worker scope, closed when the call is done with it.
struct Scope {
    launcher: Arc<dyn SandboxLauncher>,
    agent_id: String,
    scope: String,
}

impl Drop for Scope {
    fn drop(&mut self) {
        let launcher = Arc::clone(&self.launcher);
        let agent_id = std::mem::take(&mut self.agent_id);
        let scope = std::mem::take(&mut self.scope);
        clean_up_detached(
            format!("codex completion scope {scope} left to its TTL"),
            async move { launcher.close(&agent_id, CODEX_PROJECT, &scope).await },
        );
    }
}

/// A contained one-shot round's own directory, shared with the privsep
/// runner, holding its system prompt; removed with everything in it when
/// dropped.
pub(crate) struct Workdir(PathBuf);

impl Workdir {
    pub(crate) fn create(root: &Path, system: &str) -> Result<Self> {
        let workdir = Self(root.join(uuid::Uuid::new_v4().to_string()));
        crate::code::worktree::ensure_shared_dir(root)?;
        crate::code::worktree::ensure_shared_dir(workdir.path())?;
        std::fs::write(workdir.system_prompt(), system)?;
        Ok(workdir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn system_prompt(&self) -> PathBuf {
        self.0.join(SYSTEM_PROMPT_FILE)
    }
}

/// Remove the completion directories under `root` that no completion can
/// still be using: a kill skips their removal, and a sibling replica on the
/// same data directory may be mid-call in the newer ones.
pub(super) fn sweep_leftovers(root: &Path) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(dir = %root.display(), error = %e, "completion directories not swept");
            return;
        }
    };
    let cutoff = std::time::SystemTime::now() - (COMPLETION_TIMEOUT + LEFTOVER_AFTER);
    entries
        .flatten()
        .filter(|entry| {
            entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|at| at < cutoff)
        })
        .for_each(|entry| drop(Workdir(entry.path())));
}

impl Drop for Workdir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.0) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(dir = %self.0.display(), error = %e, "completion directory left behind");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use chrono::{TimeDelta, Utc};
    use workforce_agent::llm::failover::{
        classify_error, ErrorKind, FailoverConfig, FailoverProvider,
    };
    use workforce_agent::llm::streaming::{watchdogged_stream, StreamTimeouts};
    use workforce_agent::llm::types::ImageSource;
    use workforce_agent::runner::{with_served_log, CURRENT_USER_ID};
    use workforce_common::settings::{
        KnowledgeSummariserConfig, OwnerType, RetryConfig, WorkforceConfig,
    };
    use workforce_common::subscription::{CredentialTier, SubscriptionKind};
    use workforce_knowledge_service::SummariserBuilder;

    use crate::claude_code::tests::scripted::{answer, print, rejected, result, ScriptedClaude};
    use crate::claude_code::ResolvedAuth;
    use crate::subscription::test_support::Metered;
    use crate::subscription::CooldownStore;
    use crate::subscription::{
        AgentLogins, AuthJson, Credential, CredentialScope, LoginKey, Miss, Subscriptions,
        VaultSlot, Vaults,
    };
    use crate::vault::Vault;

    const KEY: &str = SubscriptionKind::Claude.vault_key();
    const ROUTE: SubscriptionRoute = SubscriptionRoute::ClaudeCode;
    const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";

    struct Hub {
        vault: Arc<Vault>,
        config: WorkforceConfig,
        dir: tempfile::TempDir,
        claude: ScriptedClaude,
    }

    impl Hub {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let vault = Arc::new(Vault::open(&dir.path().join("global.enc"), "test-key").unwrap());
            let mut config = WorkforceConfig::default();
            config.hub.data_dir = dir.path().display().to_string();
            config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
                .join("memory.db")
                .to_string_lossy()
                .into_owned();
            Self {
                vault,
                config,
                dir,
                claude: ScriptedClaude::new(),
            }
        }

        /// A hub whose own Claude login is a token of its own, so no other
        /// test's cooldown reaches it.
        fn with_login() -> (Self, String) {
            let hub = Self::new();
            let token = format!("install-{}", uuid::Uuid::new_v4());
            hub.vault.set(KEY, &token).unwrap();
            (hub, token)
        }

        fn plans(&self) -> PlanLogins {
            PlanLogins::new(
                Subscriptions::new(&self.config),
                CredentialScope::system(Arc::clone(&self.vault), &self.config.subscription),
            )
        }

        fn completion(&self) -> CliCompletion {
            CliCompletion::new(self.plans(), CliKind::ClaudeCode, Some("haiku"), None)
        }

        /// The logins of an agent `allan` on this hub, with its own `agent`
        /// vault, configured as the hub is.
        fn agent_plans(&self, agent: Option<&Arc<Vault>>) -> PlanLogins {
            let logins = AgentLogins::new(
                "allan",
                Vaults {
                    agent,
                    global: Some(&self.vault),
                },
                (None, OwnerType::User),
                Arc::new(parking_lot::RwLock::new(None)),
                Arc::new(parking_lot::RwLock::new(None)),
                crate::resolved_config::ConfigCell::new(
                    crate::resolved_config::ResolvedConfig::resolve(
                        None,
                        None,
                        &self.config,
                        "anthropic",
                        None,
                    ),
                ),
            );
            PlanLogins::new(
                Subscriptions::new(&self.config),
                CredentialScope::Agent(Arc::new(logins)),
            )
        }
    }

    /// The one round the scripted `claude` ran: the token it was given and
    /// the model it was asked for.
    fn ran(claude: &ScriptedClaude) -> (String, Option<String>) {
        let calls = claude.calls();
        assert_eq!(calls.len(), 1, "one round");
        (
            calls[0].token.clone(),
            calls[0].flag("--model").map(str::to_string),
        )
    }

    async fn ask(provider: &dyn LlmProvider) -> Result<LlmResponse> {
        provider
            .create_message(
                Some("Be terse."),
                &[LlmMessage::user("hi")],
                &[],
                None,
                None,
                false,
            )
            .await
    }

    fn subscription_error(err: &eyre::Report) -> &SubscriptionError {
        err.downcast_ref::<SubscriptionError>()
            .unwrap_or_else(|| panic!("not a subscription error: {err}"))
    }

    fn text(text: &str) -> Value {
        serde_json::json!({ "type": "text", "text": text })
    }

    fn image(data: &str) -> ContentBlock {
        ContentBlock::Image {
            source: ImageSource {
                source_type: "base64".into(),
                media_type: "image/png".into(),
                data: data.into(),
            },
        }
    }

    fn image_block(data: &str) -> Value {
        serde_json::json!({
            "type": "image",
            "source": { "type": "base64", "media_type": "image/png", "data": data },
        })
    }

    #[test]
    fn claude_input_keeps_the_newest_images_within_the_limit() {
        let message = LlmMessage {
            role: Role::User,
            content: (0..22).map(|i| image(&i.to_string())).collect(),
        };
        let blocks = render(ROUTE, &[message]).unwrap();
        let shown: Vec<&str> = blocks
            .iter()
            .filter(|b| b["type"] == "image")
            .filter_map(|b| b["source"]["data"].as_str())
            .collect();
        assert_eq!(shown.len(), image_cap::ANTHROPIC_MAX_IMAGES);
        assert_eq!(shown[0], "2");
        assert!(blocks[0]["text"]
            .as_str()
            .is_some_and(|t| t.contains("[image removed")));
    }

    #[test]
    fn claude_completion_command_is_contained() {
        let (hub, token) = Hub::with_login();
        hub.vault.set("skills/github/token", "ghp-hub").unwrap();
        let completion = CliCompletion::new(hub.plans(), CliKind::ClaudeCode, None, Some(0));
        let login = completion
            .plans
            .scope()
            .resolve(SubscriptionKind::Claude)
            .unwrap();
        let workdir = Workdir::create(
            completion.plans.subscriptions().shared().completions_dir(),
            "Be terse.",
        )
        .unwrap();
        let input = render(ROUTE, &[LlmMessage::user("hi")]).unwrap();
        let config = completion.config(&login, &workdir, input, "haiku", COMPLETION_TIMEOUT);
        let auth = ResolvedAuth::resolve(&config.auth).unwrap();
        let command = |config: &ClaudeCodeConfig| {
            claude_code::command(config, &auth, None, Some(&hub.vault), Some(&hub.vault))
        };
        let contained = command(&config);
        let cmd = contained.as_std();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let pair = |flag: &str, value: &str| args.windows(2).any(|w| w[0] == flag && w[1] == value);
        assert!(pair("--tools", ""), "no built-in tools: {args:?}");
        assert!(args.iter().any(|a| a == "--strict-mcp-config"));
        assert!(pair("--setting-sources", ""), "no settings file is read");
        let prompt = workdir.system_prompt();
        assert!(pair("--system-prompt-file", &prompt.display().to_string()));
        assert_eq!(std::fs::read_to_string(&prompt).unwrap(), "Be terse.");
        assert!(pair("--input-format", "stream-json"));
        assert!(pair("--max-turns", "1"));
        assert!(pair("--model", "haiku"));
        assert!(pair("--effort", "low"), "thinking off asks for the least");
        assert!(args.iter().any(|a| a == "--no-session-persistence"));
        for absent in [
            "--exclude-dynamic-system-prompt-sections",
            "--mcp-config",
            "--append-system-prompt",
            "--allowedTools",
            "--session-id",
            "--resume",
        ] {
            assert!(!args.iter().any(|a| a == absent), "{absent}: {args:?}");
        }
        let entry = |cmd: &tokio::process::Command, name: &str| {
            cmd.as_std()
                .get_envs()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
        };
        let env = |cmd: &tokio::process::Command, name: &str| entry(cmd, name).flatten();
        assert_eq!(env(&contained, "CLAUDE_CODE_OAUTH_TOKEN"), Some(token));
        for removed in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
        ] {
            assert_eq!(
                entry(&contained, removed),
                Some(None),
                "{removed} is removed, not just left unset"
            );
        }
        assert_eq!(
            env(&contained, "CLAUDE_CODE_DISABLE_CLAUDE_MDS").as_deref(),
            Some("1")
        );
        assert_eq!(
            env(&contained, "CLAUDE_CODE_DISABLE_AUTO_MEMORY").as_deref(),
            Some("1")
        );
        assert_eq!(cmd.get_current_dir(), Some(workdir.path()));
        assert!(workdir.path().is_absolute());
        let uncontained = command(&ClaudeCodeConfig {
            contained: None,
            ..config.clone()
        });
        assert_eq!(
            env(&uncontained, "GH_TOKEN").as_deref(),
            Some("ghp-hub"),
            "an uncontained round is handed the hub's GitHub token"
        );

        let dir = workdir.path().to_path_buf();
        drop(workdir);
        assert!(!dir.exists(), "a completion leaves nothing behind");
    }

    #[test]
    fn boot_sweeps_only_completion_dirs_no_call_can_still_use() {
        let hub = Hub::new();
        let root = Subscriptions::new(&hub.config)
            .shared()
            .completions_dir()
            .to_path_buf();
        let leftover = Workdir::create(&root, "an agent's prompt").unwrap();
        let in_flight = Workdir::create(&root, "a sibling's prompt").unwrap();
        let killed_at = std::time::SystemTime::now()
            - (COMPLETION_TIMEOUT + LEFTOVER_AFTER)
            - Duration::from_secs(1);
        std::fs::File::open(leftover.path())
            .unwrap()
            .set_modified(killed_at)
            .unwrap();
        let leftover = std::mem::ManuallyDrop::new(leftover);

        Subscriptions::boot(&hub.config, None);
        assert!(
            !leftover.path().exists(),
            "a killed completion's prompt is gone"
        );
        assert!(
            in_flight.system_prompt().exists(),
            "a call still under way keeps its directory"
        );
    }

    #[tokio::test]
    async fn cli_completion_with_tools_aborts_unsupported() {
        let hub = Hub::new();
        let completion = Arc::new(hub.completion());
        let tool = LlmToolDef {
            name: "jira.search".into(),
            description: "Search Jira".into(),
            input_schema: serde_json::json!({ "type": "object" }),
        };
        let err = completion
            .create_message(
                None,
                &[LlmMessage::user("hi")],
                std::slice::from_ref(&tool),
                None,
                None,
                false,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(subscription_error(&err), SubscriptionError::Uncarried(_)),
            "refused before any login is looked up: {err}"
        );
        assert!(err.to_string().contains("tool-less"), "{err}");
        assert_eq!(classify_error(&err.to_string()), ErrorKind::Unsupported);

        let document = LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Document {
                source: ImageSource {
                    source_type: "base64".into(),
                    media_type: "application/pdf".into(),
                    data: "JVBERi0=".into(),
                },
            }],
        };
        let err = completion
            .create_message(None, &[document], &[], None, None, false)
            .await
            .unwrap_err();
        assert_eq!(classify_error(&err.to_string()), ErrorKind::Unsupported);

        let metered = Arc::new(Metered::default());
        let chain = FailoverProvider::new(
            Arc::clone(&metered) as Arc<dyn LlmProvider>,
            FailoverConfig {
                failover: vec!["backup-model".into()],
                ..Default::default()
            },
        );
        chain
            .add_external_provider("claude-code", completion as Arc<dyn LlmProvider>)
            .await;
        let err = chain
            .create_message(
                None,
                &[LlmMessage::user("hi")],
                &[tool],
                Some("claude-code:haiku"),
                None,
                true,
            )
            .await
            .unwrap_err();
        assert_eq!(classify_error(&err.to_string()), ErrorKind::Unsupported);
        assert!(metered.0.lock().is_empty(), "a tool loop never fails over");
    }

    #[tokio::test]
    async fn claude_completion_resolves_payer_login_not_env_or_global() {
        let hub = Hub::new();
        let agent = Arc::new(Vault::open(&hub.dir.path().join("agent.enc"), "test-key").unwrap());
        let personal = crate::user_config::per_user_key(ALICE, KEY);
        let on_agent = format!("alice-agent-{}", uuid::Uuid::new_v4());
        let on_global = format!("alice-global-{}", uuid::Uuid::new_v4());
        agent.set(&personal, &on_agent).unwrap();
        hub.vault.set(&personal, &on_global).unwrap();
        hub.vault.set(KEY, "install-token").unwrap();
        let completion = CliCompletion::new(
            hub.agent_plans(Some(&agent)),
            CliKind::ClaudeCode,
            Some("haiku"),
            None,
        );

        CURRENT_USER_ID
            .scope(ALICE.to_string(), hub.claude.scoped(ask(&completion)))
            .await
            .unwrap();
        assert_eq!(
            ran(&hub.claude),
            (on_agent, Some("haiku".to_string())),
            "her own login on the agent pays"
        );
    }

    #[tokio::test]
    async fn claude_completion_notes_served_for_cost_row() {
        let (hub, token) = Hub::with_login();
        hub.claude.reply(None, &[result("Hello.")]);
        let completion = hub.completion();
        let (response, served) = with_served_log(hub.claude.scoped(ask(&completion))).await;
        let response = response.unwrap();
        assert_eq!(response.text().as_deref(), Some("Hello."));
        assert_eq!(ran(&hub.claude), (token, Some("haiku".to_string())));
        assert_eq!(response.model, "haiku");
        assert_eq!(
            (
                response.usage.input_tokens,
                response.usage.output_tokens,
                response.usage.cache_read_input_tokens
            ),
            (10, 5, 100)
        );
        let served = served.expect("the completion notes what paid for it");
        assert_eq!(served.route, SubscriptionRoute::ClaudeCode);
        assert_eq!(served.tier, CredentialTier::SystemShared);
        assert_eq!(served.ledger_model(&response.model), "claude-code:haiku");

        let (tx, mut rx) = mpsc::channel(16);
        let (streamed, served) =
            with_served_log(hub.claude.scoped(completion.create_message_stream(
                None,
                &[LlmMessage::user("hi")],
                &[],
                Some("sonnet"),
                None,
                false,
                tx,
            )))
            .await;
        streamed.unwrap();
        assert!(served.is_some(), "a streamed completion notes it too");
        let mut texts = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let StreamEvent::TextDelta { text, .. } = event {
                texts.push(text);
            }
        }
        assert_eq!(texts, ["Hello."]);
        assert_eq!(hub.claude.calls()[1].flag("--model"), Some("sonnet"));
    }

    #[tokio::test]
    async fn claude_completion_rejected_limit_cools_and_fails_over() {
        let (hub, token) = Hub::with_login();
        hub.claude.reply(None, &rejected());
        let completion = hub.completion();
        let err = hub.claude.scoped(ask(&completion)).await.unwrap_err();
        let later = Utc::now() + TimeDelta::hours(2);
        assert!(
            matches!(
                subscription_error(&err),
                SubscriptionError::LimitReached { until: Some(until), .. } if *until > later
            ),
            "cooled until the window the stream named: {err}"
        );
        assert_eq!(
            classify_error(&err.to_string()),
            ErrorKind::SubscriptionUnavailable
        );
        let key = LoginKey::of(&Credential::Token(token)).unwrap();
        assert!(CooldownStore::global()
            .until(&key, Utc::now())
            .is_some_and(|until| until > later));
        let again = hub.claude.scoped(ask(&completion)).await.unwrap_err();
        assert!(
            matches!(subscription_error(&again), SubscriptionError::NoLogin(no) if no.cooling_until().is_some()),
            "{again}"
        );

        let (fresh, _) = Hub::with_login();
        fresh.claude.reply(None, &rejected());
        let metered = Arc::new(Metered::default());
        let chain = FailoverProvider::new(
            Arc::clone(&metered) as Arc<dyn LlmProvider>,
            FailoverConfig {
                failover: vec!["backup-model".into()],
                ..Default::default()
            },
        );
        chain
            .add_external_provider("claude-code", Arc::new(fresh.completion()))
            .await;
        let response = fresh
            .claude
            .scoped(chain.create_message(
                None,
                &[LlmMessage::user("hi")],
                &[],
                Some("claude-code:haiku"),
                None,
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.text().as_deref(), Some("metered"));
        assert_eq!(*metered.0.lock(), ["backup-model"]);
    }

    #[tokio::test]
    async fn claude_completion_auth_failure_marks_dead() {
        let (hub, token) = Hub::with_login();
        hub.claude.script(
            None,
            &format!(
                "{}exit 1\n",
                print(&[serde_json::json!({
                    "type": "result",
                    "subtype": "success",
                    "is_error": true,
                    "num_turns": 1,
                    "api_error_status": 401,
                    "result": "Failed to authenticate. API Error: 401 OAuth access token is invalid.",
                })])
            ),
        );
        let completion = hub.completion();
        let err = hub.claude.scoped(ask(&completion)).await.unwrap_err();
        assert!(
            matches!(subscription_error(&err), SubscriptionError::LoginInvalid { reason, .. } if reason.contains("401")),
            "{err}"
        );
        assert_eq!(
            classify_error(&err.to_string()),
            ErrorKind::SubscriptionUnavailable
        );
        let slot = VaultSlot {
            vault: Arc::clone(&hub.vault),
            key: KEY.into(),
        };
        assert!(CooldownStore::global().is_dead(&slot, &Credential::Token(token).fingerprint()));
        let again = hub.claude.scoped(ask(&completion)).await.unwrap_err();
        assert!(
            matches!(
                subscription_error(&again),
                SubscriptionError::NoLogin(no) if no.misses.contains(&(CredentialTier::SystemShared, Miss::Invalid))
            ),
            "{again}"
        );
    }

    #[test]
    fn history_rendered_as_single_user_line_with_images_in_order() {
        let alone = LlmMessage {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "look".into(),
                },
                image("a"),
                ContentBlock::Text { text: "and".into() },
            ],
        };
        assert_eq!(
            render(ROUTE, std::slice::from_ref(&alone)).unwrap(),
            [text("look"), image_block("a"), text("and")]
        );

        let history = [
            LlmMessage {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: "what is this?".into(),
                    },
                    image("a"),
                ],
            },
            LlmMessage {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "hmm".into(),
                        signature: None,
                    },
                    ContentBlock::Text {
                        text: "a cat".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "vision.describe".into(),
                        input: serde_json::json!({ "n": 1 }),
                        signature: None,
                    },
                ],
            },
            LlmMessage {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "tabby".into(),
                        is_error: None,
                        loaded: None,
                    },
                    image("b"),
                    ContentBlock::Text {
                        text: "and this?".into(),
                    },
                ],
            },
        ];
        let blocks = render(ROUTE, &history).unwrap();
        assert_eq!(
            blocks,
            [
                text("<turn role=\"user\">\nwhat is this?"),
                image_block("a"),
                text(
                    "</turn>\n<turn role=\"assistant\">\na cat\n[tool call vision.describe: {\"n\":1}]\n</turn>\n<turn role=\"user\">\n[tool result: tabby]"
                ),
                image_block("b"),
                text("and this?\n</turn>"),
            ]
        );
        let line = Contained {
            tools: Vec::new(),
            system_prompt_file: PathBuf::from("system.md"),
            input: blocks.clone(),
            tool_timeout: None,
            exclude_dynamic: false,
        }
        .user_line();
        assert_eq!(line.matches('\n').count(), 1, "one stdin line");
        assert!(line.ends_with('\n'), "a JSON line");
        let sent: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(sent["type"], "user");
        assert_eq!(sent["message"]["role"], "user");
        assert_eq!(sent["message"]["content"], Value::Array(blocks));
    }

    #[tokio::test]
    async fn hub_claude_code_provider_serves_knowledge_and_tagger() {
        let (hub, token) = Hub::with_login();
        hub.claude.reply(None, &answer("tagged"));
        let mut config = hub.config.clone();
        config.llm.provider = "claude-code".into();
        config.llm.default_model = "cheap".into();
        let tagger = Subscriptions::new(&config)
            .tagger(&config, &hub.vault)
            .expect("a claude-code [llm] tags artifacts on the hub's login");
        let tags = hub.claude.scoped(ask(tagger.as_ref())).await.unwrap();
        assert_eq!(tags.text().as_deref(), Some("tagged"));
        assert_eq!(ran(&hub.claude), (token.clone(), Some("haiku".to_string())));

        let summariser = crate::knowledge::HubSummariserBuilder::new(KnowledgeSummariserConfig {
            provider: "claude-code".into(),
            model: "claude-code:cheap".into(),
            ..Default::default()
        })
        .with_plans(Some(hub.plans()))
        .build()
        .expect("a claude-code summariser builds on the hub's logins");
        let summary = hub
            .claude
            .scoped(summariser.generate_text("Summarise this module.", 200))
            .await
            .unwrap();
        assert_eq!(summary, "tagged");
        let summarised = &hub.claude.calls()[1];
        assert_eq!(summarised.token, token);
        assert_eq!(summarised.flag("--model"), Some("haiku"));
    }

    #[tokio::test]
    async fn cli_completion_verify_is_ok_without_payer() {
        let completion = Hub::new().completion();
        completion.verify_credentials().await.unwrap();
        assert_eq!(completion.context_floor("haiku").await, Some(200_000));
    }

    #[tokio::test(start_paused = true)]
    async fn completion_waiting_for_a_slot_names_its_failure_before_the_chain_cap() {
        let (mut hub, token) = Hub::with_login();
        hub.config.subscription.max_concurrent_completions = 1;
        let retry = RetryConfig {
            request_timeout_secs: 10,
            ..Default::default()
        };
        hub.config.llm.retry = Some(retry.clone());
        let plans = hub.agent_plans(None);
        let _held = plans
            .subscriptions()
            .shared()
            .pools()
            .completions()
            .acquire(None)
            .await
            .unwrap();
        let completion = Arc::new(CliCompletion::new(
            plans,
            CliKind::ClaudeCode,
            Some("haiku"),
            None,
        ));

        let started = tokio::time::Instant::now();
        let err = tokio::time::timeout(Duration::from_secs(60), ask(completion.as_ref()))
            .await
            .expect("gives up on its own, before the chain's cap")
            .unwrap_err();
        assert!(
            matches!(
                subscription_error(&err),
                SubscriptionError::Busy { waited, .. } if *waited == Duration::from_secs(8)
            ),
            "{err}"
        );
        assert_eq!(started.elapsed(), Duration::from_secs(8));
        assert_eq!(
            classify_error(&err.to_string()),
            ErrorKind::SubscriptionUnavailable
        );
        let key = LoginKey::of(&Credential::Token(token)).unwrap();
        assert!(
            CooldownStore::global().until(&key, Utc::now()).is_none(),
            "a busy hub cools no login"
        );

        let metered = Arc::new(Metered::default());
        let chain = FailoverProvider::new(
            Arc::clone(&metered) as Arc<dyn LlmProvider>,
            FailoverConfig {
                failover: vec!["backup-model".into()],
                retry: workforce_agent::llm::failover::RetryConfig {
                    timeouts: (&retry).into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        chain
            .add_external_provider("claude-code", completion as Arc<dyn LlmProvider>)
            .await;
        let started = tokio::time::Instant::now();
        let response = chain
            .create_message(
                None,
                &[LlmMessage::user("hi")],
                &[],
                Some("claude-code:haiku"),
                None,
                true,
            )
            .await
            .unwrap();
        assert_eq!(response.text().as_deref(), Some("metered"));
        assert_eq!(*metered.0.lock(), ["backup-model"]);
        assert_eq!(started.elapsed(), Duration::from_secs(8));
    }

    #[tokio::test(start_paused = true)]
    async fn completion_waits_for_permit_without_first_token_timeout() {
        let (mut hub, _) = Hub::with_login();
        hub.config.subscription.max_concurrent_completions = 1;
        let plans = hub.plans();
        let _held = plans
            .subscriptions()
            .shared()
            .pools()
            .completions()
            .acquire(None)
            .await
            .unwrap();
        let completion = CliCompletion::new(plans, CliKind::ClaudeCode, Some("haiku"), None);
        let (tx, mut rx) = mpsc::channel(64);
        let messages = [LlmMessage::user("hi")];
        let limits = StreamTimeouts {
            first_token: Duration::from_secs(45),
            stall: Duration::from_secs(45),
        };
        let waited = tokio::time::timeout(
            Duration::from_secs(200),
            watchdogged_stream(limits, "haiku", tx, |inner| {
                completion.create_message_stream(None, &messages, &[], None, None, false, inner)
            }),
        )
        .await;
        assert!(
            waited.is_err(),
            "still waiting for its permit, not timed out: {waited:?}"
        );
        let mut waits = 0;
        while let Ok(event) = rx.try_recv() {
            assert!(
                matches!(event, StreamEvent::KeepAlive { within_secs: 30 }),
                "{event:?}"
            );
            waits += 1;
        }
        assert!(waits >= 6, "kept alive every 30 s: {waits}");
    }

    /// A hub whose own ChatGPT login is a sign-in of its own, with its Codex
    /// completions seated on `seats`, run by the scripted `codex`.
    fn codex_hub(seats: &Arc<crate::coding_testing::Seats>) -> (Hub, PlanLogins, String) {
        let hub = Hub::new();
        let login = crate::subscription::test_support::login(
            &format!("acct-{}", uuid::Uuid::new_v4()),
            "a0",
            "rt-0",
        );
        hub.vault
            .set(SubscriptionKind::ChatGpt.vault_key(), &login)
            .unwrap();
        let plans = PlanLogins::new(
            Subscriptions::new(&hub.config)
                .with_codex_worker(Arc::clone(seats) as Arc<dyn SandboxLauncher>),
            CredentialScope::system(Arc::clone(&hub.vault), &hub.config.subscription),
        );
        (hub, plans, login)
    }

    fn local_seats() -> Arc<crate::coding_testing::Seats> {
        Arc::new(crate::coding_testing::Seats {
            local: true,
            ..Default::default()
        })
    }

    async fn with_codex<T>(
        codex: &crate::coding_engine::codex::tests::ScriptedCodex,
        work: impl std::future::Future<Output = T>,
    ) -> T {
        crate::coding_engine::codex::tests::CODEX
            .scope(codex.program(), work)
            .await
    }

    #[tokio::test]
    async fn codex_completion_runs_contained_in_empty_scope_and_closes_it() {
        use crate::coding_engine::codex::tests::{ScriptedCodex, SCRIPTED_ANSWER};
        let seats = local_seats();
        let (_hub, plans, _) = codex_hub(&seats);
        let codex = ScriptedCodex::new();
        let completion = CliCompletion::new(plans, CliKind::Codex, None, Some(0));

        let (response, served) = with_served_log(with_codex(&codex, ask(&completion))).await;
        let response = response.unwrap();
        assert!(
            matches!(response.content.as_slice(), [ContentBlock::Text { text }] if text == SCRIPTED_ANSWER),
            "{:?}",
            response.content
        );
        assert_eq!(response.usage.output_tokens, 7);
        assert_eq!(served.map(|s| s.route), Some(SubscriptionRoute::Codex));

        assert_eq!(codex.runs(), 1);
        let config: toml::Table = codex.recorded(1, "config").parse().unwrap();
        assert_eq!(config["model"].as_str(), Some("gpt-6.1-sol"));
        assert_eq!(config["sandbox_mode"].as_str(), Some("read-only"));
        assert_eq!(
            config["shell_environment_policy"]["inherit"].as_str(),
            Some("none")
        );
        assert_eq!(config["features"]["shell_tool"].as_bool(), Some(false));
        assert!(config.get("mcp_servers").is_none());
        let args = codex.recorded(1, "args");
        assert!(args.lines().any(|a| a == "--ephemeral"), "{args}");
        assert_eq!(codex.recorded(1, "stdin"), "hi");
        assert_eq!(
            codex.recorded(1, "instructions"),
            "Be terse.",
            "the system prompt replaces Codex's own instructions"
        );
        assert!(config["model_instructions_file"]
            .as_str()
            .is_some_and(|file| file.ends_with("instructions.md")));
        let ensured = seats.ensured.lock().unwrap().clone();
        assert_eq!(ensured.len(), 1);
        let (project, scope, home) = &ensured[0];
        assert_eq!(project, CODEX_PROJECT);
        assert_eq!(*home, HomeSpec::Ephemeral);
        let workdir = std::path::PathBuf::from(codex.recorded(1, "cwd").trim());
        assert_eq!(
            std::fs::read_dir(&workdir).unwrap().count(),
            0,
            "an empty scope"
        );
        for _ in 0..50 {
            if !seats.closed.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(*seats.closed.lock().unwrap(), std::slice::from_ref(scope));
    }

    #[tokio::test]
    async fn codex_completion_writes_refreshed_login_back() {
        use crate::coding_engine::codex::tests::ScriptedCodex;
        let seats = local_seats();
        let (hub, plans, handed) = codex_hub(&seats);
        let codex = ScriptedCodex::new();
        let account = AuthJson::parse(&handed).unwrap().account_id().unwrap();
        let refreshed = crate::subscription::test_support::login(&account, "a1", "rt-1");
        codex.refreshes_to(&refreshed);
        let completion = CliCompletion::new(plans, CliKind::Codex, None, None);

        with_codex(&codex, ask(&completion)).await.unwrap();

        assert_eq!(
            AuthJson::parse(&codex.recorded(1, "auth"))
                .unwrap()
                .refresh_token(),
            Some("rt-0"),
            "the round was handed the stored login"
        );
        hub.vault.sync().unwrap();
        let stored = hub
            .vault
            .get(SubscriptionKind::ChatGpt.vault_key())
            .unwrap();
        assert_eq!(
            AuthJson::parse(&stored).unwrap().refresh_token(),
            Some("rt-1"),
            "the CLI's refresh is kept where the login came from"
        );
    }

    #[tokio::test]
    async fn codex_completion_refuses_images_and_codex_vision_refused_at_load() {
        let seats = local_seats();
        let (_hub, plans, _) = codex_hub(&seats);
        let completion = CliCompletion::new(plans, CliKind::Codex, None, None);
        let err = completion
            .create_message(
                None,
                &[LlmMessage {
                    role: Role::User,
                    content: vec![
                        ContentBlock::Text {
                            text: "what is this?".into(),
                        },
                        image("aGk="),
                    ],
                }],
                &[],
                None,
                None,
                false,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(subscription_error(&err), SubscriptionError::Uncarried(reason) if reason.contains("no images")),
            "{err}"
        );
        assert!(
            seats.ensured.lock().unwrap().is_empty(),
            "refused before a seat"
        );

        let llm = workforce_common::settings::LlmConfig {
            vision: Some(toml::from_str("model = \"codex:gpt-6.1-sol\"").unwrap()),
            ..Default::default()
        };
        let mut config = WorkforceConfig::default();
        config.coding_session.executor.kind = workforce_common::settings::ExecutorKind::Remote;
        let resolved =
            crate::resolved_config::ResolvedConfig::resolve(None, None, &config, "anthropic", None);
        let refused = crate::subscription::validate_agent_llm(
            &llm,
            &resolved,
            &crate::subscription::Install::of(&config),
        )
        .unwrap_err();
        assert!(refused.to_string().contains("no images"), "{refused}");
    }
}
