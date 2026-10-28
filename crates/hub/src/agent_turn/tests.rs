//! Agent turns on their routes, against a scripted `claude`, a scripted
//! `codex` and a mock ChatGPT backend.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use workforce_agent::llm::failover::{FailoverConfig, FailoverProvider, HealthConfig};
use workforce_agent::llm::provider::ProviderKind;
use workforce_agent::llm::types::{ContentBlock, ImageSource};
use workforce_agent::runner::{InjectedMessage, ProgressEvent, RunResult, StreamDiscardReason};
use workforce_common::settings::{ExecutorKind, WorkforceConfig};
use workforce_common::subscription::SubscriptionKind;

use super::*;
use crate::claude_code::tests::scripted::{
    answer, assistant, print, rejected, rejected_until, result, ScriptedClaude,
};
use crate::cli_session::RotationPolicy;
use crate::coding_engine::codex::tests::{
    completed_event, failed_event, message_event, ScriptedCodex, SCRIPTED_ANSWER,
};
use crate::coding_sandbox::{LocalLauncher, McpSurface};
use crate::coding_testing::Seats;
use crate::projects::layout::WorkRoots;
use crate::projects::setup::SetupRunner;
use crate::serve::testing::{answer_bearer, chatgpt_service_on, store_chatgpt_login};
use crate::serve::AgentService;
use crate::subscription::test_support::SKY;

pub(crate) const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
const SLOT: &str = "agent_engine:claude-code";

/// A hub whose agent runs its turns on `claude-code:sonnet`, then
/// `chatgpt:gpt-6.1-sol`, unless `configure` says otherwise.
pub(crate) struct Hub {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) server: wiremock::MockServer,
    pub(crate) service: Arc<AgentService>,
    pub(crate) claude: ScriptedClaude,
    pub(crate) codex: ScriptedCodex,
    /// Where its Codex turns are seated, when it has a coding worker.
    pub(crate) seats: Option<Arc<Seats>>,
}

pub(crate) fn claude_first(config: &mut WorkforceConfig) {
    config.llm.provider = "claude-code".into();
    config.llm.default_model = "sonnet".into();
    config.llm.failover = vec!["chatgpt:gpt-6.1-sol".into()];
}

pub(crate) async fn hub(configure: impl FnOnce(&mut WorkforceConfig)) -> Hub {
    seated(configure, None).await
}

/// Codex first, on its worker, then `claude-code:sonnet`.
fn codex_first(config: &mut WorkforceConfig) {
    config.coding_session.executor.kind = ExecutorKind::Remote;
    config.llm.provider = "codex".into();
    config.llm.default_model = "gpt-6.1-sol".into();
    config.llm.failover = vec!["claude-code:sonnet".into()];
}

/// A hub with a coding worker, its Codex turns seated in workspaces of
/// their own on this host.
async fn codex_hub(configure: impl FnOnce(&mut WorkforceConfig)) -> Hub {
    seated(
        configure,
        Some(Arc::new(Seats {
            local: true,
            ..Seats::default()
        })),
    )
    .await
}

async fn seated(configure: impl FnOnce(&mut WorkforceConfig), seats: Option<Arc<Seats>>) -> Hub {
    let dir = tempfile::tempdir().unwrap();
    let server = wiremock::MockServer::start().await;
    let service = chatgpt_service_on(
        dir.path(),
        &server,
        configure,
        seats
            .clone()
            .map(|seats| seats as Arc<dyn crate::coding_sandbox::SandboxLauncher>),
    )
    .await;
    let claude = ScriptedClaude::new();
    let codex = ScriptedCodex::new();
    let launcher = LocalLauncher::new(
        WorkRoots {
            data_dir: dir.path().join("work"),
            hot: None,
        },
        SetupRunner {
            runner: None,
            env: Vec::new(),
            timeout: Duration::from_secs(60),
        },
        McpSurface {
            exposed_tools: None,
        },
    );
    service.turn_rig.set(Arc::new(
        AgentTurnRig::new(
            Arc::new(launcher),
            seats
                .clone()
                .map(|seats| seats as Arc<dyn crate::coding_sandbox::SandboxLauncher>),
            RotationPolicy::default(),
            Duration::from_secs(3600),
        )
        .with_program(CliKind::ClaudeCode, claude.program())
        .with_program(CliKind::Codex, codex.program()),
    ));
    Hub {
        dir,
        server,
        service,
        claude,
        codex,
        seats,
    }
}

impl Hub {
    /// `person`'s own Claude login on the agent, a token of its own so no
    /// other test's cooldown reaches it; returns the token.
    pub(crate) fn claude_login(&self, person: &str) -> String {
        let token = format!("{person}-{}", uuid::Uuid::new_v4());
        self.service
            .vault
            .as_ref()
            .unwrap()
            .set(
                &crate::user_config::per_user_key(person, SubscriptionKind::Claude.vault_key()),
                &token,
            )
            .unwrap();
        token
    }

    /// `person`'s ChatGPT login, answered by the backend.
    pub(crate) async fn chatgpt_login(&self, person: &str) {
        let access = store_chatgpt_login(&self.service, person);
        answer_bearer(&self.server, &access).await;
    }

    pub(crate) async fn chatgpt_requests(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/responses")
            .count()
    }

    /// A turn from `user` in the web chat, answered in full.
    pub(crate) async fn ask(&self, user: &str, message: &str) -> eyre::Result<String> {
        self.service
            .process_message(
                "web", user, message, None, None, None, None, None, None, None, None, None, None,
            )
            .await
    }

    /// `user`'s web session.
    fn session(&self, user: &str) -> workforce_common::session::Session {
        self.service
            .sessions
            .get_or_create("web", user, self.service.agent_id.as_str())
            .unwrap()
    }

    /// A streamed turn from `user`, fed `inject`: every event it emitted and
    /// what it returned.
    async fn stream(
        &self,
        user: &str,
        message: &str,
        inject: Option<mpsc::Receiver<InjectedMessage>>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> (Vec<ProgressEvent>, eyre::Result<RunResult>) {
        self.stream_at(("web", None, None), user, message, inject, cancel)
            .await
    }

    /// A streamed turn from `user` on `channel`, in the Slack channel and
    /// thread given.
    async fn stream_at(
        &self,
        place: (&str, Option<&str>, Option<&str>),
        user: &str,
        message: &str,
        inject: Option<mpsc::Receiver<InjectedMessage>>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> (Vec<ProgressEvent>, eyre::Result<RunResult>) {
        let progress = self.start(place, user, message, None, inject, cancel).await;
        let mut rx = progress.progress_rx;
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        (events, progress.handle.await.unwrap())
    }

    /// A turn from `user` on `channel` with `attachments`, under way.
    async fn start(
        &self,
        (channel, channel_id, thread): (&str, Option<&str>, Option<&str>),
        user: &str,
        message: &str,
        attachments: Option<Vec<workforce_common::session::SessionAttachment>>,
        inject: Option<mpsc::Receiver<InjectedMessage>>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> crate::serve::MessageProgress {
        self.service
            .process_message_with_progress(
                channel,
                user,
                None,
                message,
                None,
                None,
                attachments,
                None,
                channel_id,
                thread,
                inject.map(|rx| crate::serve::TurnInbox {
                    rx,
                    self_serve: workforce_agent::tools::approval::SelfServe::Answerable,
                }),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                cancel,
                None,
            )
            .await
            .unwrap()
    }
}

#[test]
fn turn_chain_groups_cli_entries_and_runner_runs_in_configured_order() {
    let spec = |raw: &str| {
        workforce_agent::llm::failover::routed_spec(
            &workforce_agent::llm::tiers::TierRegistry::new(None),
            raw,
        )
    };
    let chain = TurnChain::group(
        [
            "chatgpt:gpt-6.1-sol",
            "openrouter:openai/gpt-5",
            "claude-code:sonnet",
            "gemini:gemini-3.1-pro",
            "claude-code:opus",
        ]
        .into_iter()
        .map(spec)
        .collect(),
        ProviderKind::Gemini,
    );
    assert_eq!(
        chain.routes(),
        [
            TurnRoute::Runner(vec![
                "chatgpt:gpt-6.1-sol".into(),
                "openrouter:openai/gpt-5".into()
            ]),
            TurnRoute::Cli {
                cli: CliKind::ClaudeCode,
                model: "sonnet".into()
            },
            TurnRoute::Runner(vec!["gemini:gemini-3.1-pro".into()]),
            TurnRoute::Cli {
                cli: CliKind::ClaudeCode,
                model: "opus".into()
            },
        ]
    );
    assert!(!chain.runner_only());
    assert_eq!(
        chain.media_model("chatgpt:gpt-6.1-sol"),
        "chatgpt:gpt-6.1-sol"
    );

    let on_primary = TurnChain::group(vec![spec("sonnet")], ProviderKind::ClaudeCode);
    assert_eq!(
        on_primary.routes(),
        [TurnRoute::Cli {
            cli: CliKind::ClaudeCode,
            model: "sonnet".into()
        }],
        "an unprefixed entry runs on the agent's primary"
    );
    assert_eq!(on_primary.media_model("sonnet"), "claude-code:sonnet");
    assert!(TurnChain::group(vec![spec("gpt-6.1-sol")], ProviderKind::ChatGpt).runner_only());
}

#[tokio::test]
async fn degraded_chatgpt_stays_ahead_of_claude_code_route() {
    let chain = FailoverProvider::new(
        Arc::new(crate::subscription::test_support::Metered::default()),
        FailoverConfig {
            failover: vec!["claude-code:sonnet".into()],
            health: HealthConfig {
                failure_threshold: 1,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    chain
        .health_tracker()
        .record_failure("chatgpt:gpt-6.1-sol")
        .await;
    assert!(!chain.is_model_healthy("chatgpt:gpt-6.1-sol").await);
    let routes = TurnChain::group(
        chain.resolve_chain(Some("chatgpt:gpt-6.1-sol")).await,
        ProviderKind::Gemini,
    );
    assert_eq!(
        routes.routes()[0],
        TurnRoute::Runner(vec!["chatgpt:gpt-6.1-sol".into()]),
        "a degraded runner entry is reordered only inside its own route"
    );
}

#[tokio::test]
async fn process_message_sync_serves_claude_code_route() {
    let hub = hub(claude_first).await;
    let token = hub.claude_login(ALICE);
    hub.claude.reply(None, &answer("Claude answers."));

    let reply = hub.ask(ALICE, "hello there").await.unwrap();

    assert_eq!(reply, "Claude answers.");
    let calls = hub.claude.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].token, token, "the payer's own plan");
    assert_eq!(hub.chatgpt_requests().await, 0);
    let session = hub.session(ALICE);
    let last = session.messages.last().unwrap();
    assert_eq!(last.content, "Claude answers.");
    let slot = session.engine_slot(SLOT).expect("the transcript is kept");
    assert_eq!(slot.turns, 1);
    assert_eq!(slot.synced_through.as_deref(), Some(last.id.as_str()));
    assert_eq!(
        calls[0].flag("--session-id"),
        Some(slot.id.to_string().as_str())
    );
}

#[tokio::test]
async fn process_message_with_progress_serves_claude_code_route() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.claude.reply(None, &answer("Streamed from Claude."));

    let (events, result) = hub.stream(ALICE, "hello", None, None).await;

    let result = result.unwrap();
    assert_eq!(result.response, "Streamed from Claude.");
    assert_eq!(result.model, "claude-code:sonnet");
    assert!(events.iter().any(|e| matches!(
        e,
        ProgressEvent::Response { text } if text == "Streamed from Claude."
    )));
    let session = hub.session(ALICE);
    assert_eq!(
        session.messages.last().unwrap().content,
        "Streamed from Claude."
    );
    assert_eq!(session.engine_slot(SLOT).unwrap().turns, 1);
}

#[tokio::test]
async fn streamed_cli_turn_carries_what_served_it() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.claude.reply(None, &answer("Streamed from Claude."));

    let progress = hub
        .start(("web", None, None), ALICE, "hello", None, None, None)
        .await;
    let mut rx = progress.progress_rx;
    while rx.recv().await.is_some() {}
    let result = progress.handle.await.unwrap().unwrap();

    let served = progress.served.served().expect("the plan served the turn");
    assert_eq!(
        served.tier,
        workforce_common::subscription::CredentialTier::AgentUser
    );
    assert_eq!(
        crate::subscription::route_line(&hub.service, &result.model, Some(&served)).as_deref(),
        Some("Claude Code · sonnet · someone's plan (this agent)")
    );
    assert_eq!(
        crate::subscription::route_line(&hub.service, "gemini-3.1-pro", None).as_deref(),
        Some("Metered · gemini-3.1-pro"),
        "a metered turn on an agent that runs on a plan says so"
    );
}

#[tokio::test]
async fn claude_turn_args_system_prompt_toolsearch_strict_mcp_setting_sources_stream_json() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);

    hub.ask(ALICE, "hello").await.unwrap();

    let call = &hub.claude.calls()[0];
    assert_eq!(call.flag("--tools"), Some("ToolSearch"));
    assert!(call.has("--strict-mcp-config"));
    assert_eq!(
        call.flag("--setting-sources"),
        Some(""),
        "no settings file is read, so no hook of one runs"
    );
    assert!(
        call.has("--exclude-dynamic-system-prompt-sections"),
        "a turn's system prompt stays cacheable across turns"
    );
    assert_eq!(call.flag("--input-format"), Some("stream-json"));
    assert_eq!(call.flag("--model"), Some("sonnet"));
    assert!(call.flag("--max-turns").is_some());
    assert!(call.flag("--mcp-config").is_some());
    assert!(call
        .flag("--allowedTools")
        .is_some_and(|tools| tools.contains("mcp__workforce__")));
    assert!(
        call.prompt.contains("mcp__workforce__"),
        "the agent's own prompt, told how its tools are named"
    );
    assert!(!call.prompt.contains("<!-- CACHE_BREAK -->"));
    for absent in [
        "--append-system-prompt",
        "--bare",
        "--dangerously-skip-permissions",
    ] {
        assert!(!call.has(absent), "{absent}");
    }
    assert!(!call
        .args
        .windows(2)
        .any(|w| w[0] == "--tools" && w[1].is_empty()));
}

#[tokio::test]
async fn stream_json_stdin_carries_image_blocks_in_anthropic_shape() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    let rig = hub.service.turn_rig.get::<AgentTurnRig>().unwrap();
    let image = ContentBlock::Image {
        source: ImageSource {
            source_type: "base64".into(),
            media_type: "image/png".into(),
            data: "iVBORw0KGgo=".into(),
        },
    };
    let attempt = workforce_agent::runner::CURRENT_USER_ID
        .scope(
            ALICE.to_string(),
            rig.run(
                &hub.service,
                cli_turn(
                    vec![
                        image,
                        ContentBlock::Text {
                            text: "What colour is the word?".into(),
                        },
                    ],
                    &hub.service,
                ),
                None,
                None,
            ),
        )
        .await
        .unwrap();
    assert!(matches!(attempt, CliAttempt::Served(_)));

    let stdin = &hub.claude.calls()[0].stdin;
    assert_eq!(stdin["type"], "user");
    assert_eq!(stdin["message"]["role"], "user");
    assert_eq!(
        stdin["message"]["content"],
        json!([
            {
                "type": "image",
                "source": { "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo=" },
            },
            { "type": "text", "text": "What colour is the word?" },
        ])
    );
}

/// A turn on `claude-code:sonnet` with `task`, offered no tools, for Alice.
fn cli_turn(task: Vec<ContentBlock>, service: &AgentService) -> CliTurn {
    CliTurn {
        cli: CliKind::ClaudeCode,
        model: "sonnet".into(),
        system_prompt: "You are allan.".into(),
        task,
        tools: TurnTools {
            registry: Arc::clone(&service.tools),
            exclusive: false,
        },
        max_turns: 8,
        principal: ALICE.into(),
        oversight: Oversight::Unattended,
        self_serve: workforce_agent::tools::approval::SelfServe::Answerable,
        journal: None,
        progress: None,
        cancel: tokio_util::sync::CancellationToken::new(),
        timeout: AGENT_TURN_TIMEOUT,
    }
}

#[tokio::test]
async fn no_claude_login_hops_to_chatgpt_without_spawning() {
    let hub = hub(claude_first).await;
    hub.chatgpt_login(ALICE).await;

    let reply = hub.ask(ALICE, "why is the sky blue?").await.unwrap();

    assert_eq!(reply, SKY);
    assert!(
        hub.claude.calls().is_empty(),
        "no CLI is spawned without a login"
    );
    assert_eq!(hub.chatgpt_requests().await, 1);
    assert!(
        hub.session(ALICE).engine_slot(SLOT).is_none(),
        "a runner-served turn keeps no transcript"
    );
}

#[tokio::test]
async fn rejected_rate_limit_event_cools_login_and_hops_before_output() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.chatgpt_login(ALICE).await;
    hub.claude.reply(None, &rejected());

    let (events, result) = hub.stream(ALICE, "why is the sky blue?", None, None).await;

    assert_eq!(result.unwrap().response, SKY);
    assert_eq!(hub.claude.calls().len(), 1);
    assert!(
        !events.iter().any(|e| matches!(
            e,
            ProgressEvent::StreamDelta { text } if text.contains("session limit")
        )),
        "the CLI's own error message is never shown"
    );
    let cooling = workforce_agent::runner::CURRENT_USER_ID
        .sync_scope(ALICE.to_string(), || {
            hub.service.logins.resolve(SubscriptionKind::Claude)
        })
        .unwrap_err();
    assert!(cooling.cooling_until().is_some(), "{cooling}");
}

#[tokio::test]
async fn cooling_login_skipped_until_reset() {
    let hub = hub(claude_first).await;
    let token = hub.claude_login(ALICE);
    hub.chatgpt_login(ALICE).await;
    let key =
        crate::subscription::LoginKey::of(&crate::subscription::Credential::Token(token)).unwrap();
    crate::subscription::CooldownStore::global().note(
        &key,
        &crate::subscription::LimitSignal {
            status: crate::subscription::LimitStatus::Rejected,
            resets_at: Some(chrono::Utc::now() + chrono::TimeDelta::seconds(2)),
            window: None,
        },
        chrono::Utc::now(),
    );

    assert_eq!(hub.ask(ALICE, "first").await.unwrap(), SKY);
    assert!(
        hub.claude.calls().is_empty(),
        "a cooling login is skipped unspawned"
    );

    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(hub.ask(ALICE, "second").await.unwrap(), "default answer");
    assert_eq!(
        hub.claude.calls().len(),
        1,
        "after the reset it serves again"
    );
}

#[tokio::test]
async fn failure_after_output_is_final_not_handed_over() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.chatgpt_login(ALICE).await;
    hub.claude.script(
        None,
        &format!(
            "{}exit 1\n",
            print(&[
                assistant("msg-1", "Looking into it now."),
                json!({
                    "type": "result",
                    "subtype": "success",
                    "is_error": true,
                    "num_turns": 2,
                    "result": "API Error: 500 Internal server error",
                }),
            ])
        ),
    );

    let err = hub.ask(ALICE, "why is the sky blue?").await.unwrap_err();

    assert!(err.to_string().contains("500"), "{err:#}");
    assert_eq!(
        hub.chatgpt_requests().await,
        0,
        "a route that produced output is never handed over"
    );
}

#[tokio::test]
async fn model_text_mentioning_401_never_marks_the_login_dead() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.claude.script(
        None,
        &format!(
            "{}exit 1\n",
            print(&[assistant(
                "msg-1",
                "Your API answers 401 Unauthorized because the token expired."
            )])
        ),
    );

    let err = hub
        .ask(ALICE, "why does our API say 401?")
        .await
        .unwrap_err();

    assert!(
        !err.to_string().contains("signed in again"),
        "a crash is not a refused login: {err:#}"
    );
    workforce_agent::runner::CURRENT_USER_ID
        .sync_scope(ALICE.to_string(), || {
            hub.service.logins.resolve(SubscriptionKind::Claude)
        })
        .expect("the model's own words never mark the login dead");
}

#[tokio::test]
async fn turn_progress_matches_runner_shape_one_response_one_done() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.claude.reply(
        None,
        &[
            json!({
                "type": "assistant",
                "message": { "id": "msg-1", "content": [{ "type": "thinking", "thinking": "" }] },
            }),
            assistant("msg-1", "Let me look."),
            json!({
                "type": "assistant",
                "message": { "id": "msg-1", "content": [{ "type": "tool_use", "name": "mcp__workforce__web_fetch", "input": {} }] },
            }),
            assistant("msg-2", "Found it."),
            result("Found it."),
        ],
    );

    let (events, result) = hub.stream(ALICE, "look it up", None, None).await;
    result.unwrap();

    let shape: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            ProgressEvent::LlmCall { round } => Some(format!("call {round}")),
            ProgressEvent::StreamStart => Some("start".into()),
            ProgressEvent::StreamDelta { text } => Some(format!("delta {text}")),
            ProgressEvent::StreamEnd => Some("end".into()),
            ProgressEvent::Response { text } => Some(format!("response {text}")),
            ProgressEvent::Done { .. } => Some("done".into()),
            ProgressEvent::ToolStart { name, .. } => Some(format!("tool {name}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        shape,
        [
            "call 1",
            "start",
            "delta Let me look.",
            "end",
            "call 2",
            "start",
            "delta Found it.",
            "end",
            "response Found it.",
            "done",
        ]
    );
}

#[tokio::test]
async fn cli_turn_response_is_final_message_not_narration() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.claude.reply(
        None,
        &[
            assistant("msg-1", "I'll check the docs first."),
            assistant("msg-2", "The answer is 42."),
            result("The answer is 42."),
        ],
    );

    assert_eq!(
        hub.ask(ALICE, "question").await.unwrap(),
        "The answer is 42."
    );
}

/// run3's result: a resumed session's usage is the invocation's own, while
/// its `total_cost_usd` is the whole transcript's.
fn resumed_result() -> Value {
    json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "num_turns": 1,
        "result": "HELLO in red.",
        "usage": {
            "input_tokens": 10,
            "cache_creation_input_tokens": 192,
            "cache_read_input_tokens": 8326,
            "output_tokens": 117,
        },
        "total_cost_usd": 0.0191486,
    })
}

#[tokio::test]
async fn cli_turn_cost_uses_per_invocation_usage() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.claude.reply(
        None,
        &[assistant("msg-1", "HELLO in red."), resumed_result()],
    );

    let (_, result) = hub.stream(ALICE, "what colour?", None, None).await;

    let usage = result.unwrap().total_usage;
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens
        ),
        (10, 117, 192, 8326)
    );
}

#[tokio::test]
async fn cli_served_turn_records_ledger_key_billing_and_route_telemetry() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.claude.reply(
        None,
        &[assistant("msg-1", "HELLO in red."), resumed_result()],
    );

    hub.service
        .process_message(
            "web",
            ALICE,
            "what colour?",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("cli-turn-1"),
            None,
            Some(ALICE),
        )
        .await
        .unwrap();

    let trace = hub
        .service
        .cost_tracker
        .telemetry_store()
        .unwrap()
        .get_task_trace("cli-turn-1")
        .unwrap()
        .expect("the turn's trace");
    let row = trace
        .cost_records
        .iter()
        .find(|row| row.model == "claude-code:sonnet")
        .unwrap_or_else(|| panic!("a claude-code:sonnet row in {:?}", trace.cost_records));
    assert_eq!(
        row.billing,
        workforce_common::subscription::Billing::Subscription
    );
    assert_eq!(row.payer.as_deref(), Some(ALICE));
    assert_eq!(
        row.credential_tier
            .map(workforce_common::subscription::CredentialTier::as_str),
        Some("agent_user")
    );
    assert_eq!(
        (row.input_tokens, row.output_tokens),
        (10, 117),
        "the invocation's own usage"
    );
    let priced = hub
        .service
        .cost_tracker
        .calculate_cost("claude-code:sonnet", 10, 117, 192, 8326);
    assert!(
        (row.cost_usd - priced).abs() < 1e-9,
        "{} vs {priced}",
        row.cost_usd
    );
    assert_eq!(trace.task.provider, "claude-code");
    assert_eq!(trace.task.model, "claude-code:sonnet");
}

#[tokio::test]
async fn cli_turn_skips_observed_usage_calibration() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);

    hub.ask(ALICE, "hello").await.unwrap();

    let session = hub.session(ALICE);
    for key in ["observed_input_tokens", "estimate_calibration"] {
        assert!(
            session.metadata.get(key).is_none(),
            "a CLI's transcript tokens never calibrate the session's estimate: {key}"
        );
    }
}

#[tokio::test]
async fn engine_slot_resumes_rotates_and_recovers_session_not_found_once() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);

    hub.ask(ALICE, "one").await.unwrap();
    hub.ask(ALICE, "two").await.unwrap();
    let calls = hub.claude.calls();
    let id = calls[0].flag("--session-id").unwrap().to_string();
    assert!(!calls[0].has("--resume"));
    assert_eq!(
        calls[1].flag("--resume"),
        Some(id.as_str()),
        "the kept transcript resumes"
    );
    assert_eq!(hub.session(ALICE).engine_slot(SLOT).unwrap().turns, 2);

    let mut session = hub.session(ALICE);
    let worn = EngineSlot {
        turns: RotationPolicy::default().turns,
        ..session.engine_slot(SLOT).unwrap()
    };
    session.set_engine_slot(SLOT, &worn);
    hub.service.sessions.save(&session).unwrap();
    hub.ask(ALICE, "three").await.unwrap();
    let third = &hub.claude.calls()[2];
    assert!(!third.has("--resume"), "a worn transcript is replaced");
    let rotated = third.flag("--session-id").unwrap().to_string();
    assert_ne!(rotated, id);

    hub.claude.script(
        Some(4),
        "echo 'No conversation found with session ID' >&2\nexit 1\n",
    );
    assert_eq!(hub.ask(ALICE, "four").await.unwrap(), "default answer");
    let calls = hub.claude.calls();
    assert_eq!(calls.len(), 5, "one recovery, no more");
    assert_eq!(calls[3].flag("--resume"), Some(rotated.as_str()));
    assert!(!calls[4].has("--resume"));
    assert!(
        calls[4].text().contains("[CONVERSATION SO FAR"),
        "the fresh transcript is caught up"
    );
    let slot = hub.session(ALICE).engine_slot(SLOT).unwrap();
    assert_eq!(slot.id.to_string(), calls[4].flag("--session-id").unwrap());
    assert_eq!(slot.turns, 1);
}

#[tokio::test]
async fn catch_up_covers_turns_other_routes_served() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.chatgpt_login(ALICE).await;
    hub.ask(ALICE, "first on claude").await.unwrap();

    hub.claude.reply(
        Some(2),
        &rejected_until(chrono::Utc::now() + chrono::TimeDelta::seconds(2)),
    );
    assert_eq!(hub.ask(ALICE, "why is the sky blue?").await.unwrap(), SKY);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    hub.ask(ALICE, "third, back on claude").await.unwrap();

    let calls = hub.claude.calls();
    let third = calls.last().unwrap();
    assert!(third.has("--resume"));
    let text = third.text();
    assert!(text.contains("why is the sky blue?"), "{text}");
    assert!(text.contains(SKY), "{text}");
    assert!(
        !text.contains("first on claude"),
        "what the transcript already holds is not repeated: {text}"
    );
    assert!(text.ends_with("third, back on claude"), "{text}");
}

#[tokio::test]
async fn catch_up_skips_messages_with_no_text() {
    let hub = hub(claude_first).await;
    let mut session = hub.session(ALICE);
    session.add_user_message("look it up", None);
    session.add_assistant_message("Found it.", 0);
    session.add_user_message("", None);
    if let Some(bare) = session.messages.last_mut() {
        bare.attachments = Some(Vec::new());
    }
    session.add_user_message("thanks", None);

    let text = conversation::catch_up(&session, None, None).unwrap();

    assert!(text.contains("user: look it up") && text.contains("you: Found it."));
    assert!(
        !text
            .lines()
            .any(|line| matches!(line.trim(), "user:" | "you:")),
        "a message with nothing to read carries no line: {text}"
    );
}

#[tokio::test]
async fn catch_up_is_in_task_not_system_prompt() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.chatgpt_login(ALICE).await;
    hub.ask(ALICE, "hello").await.unwrap();
    let mut session = hub.session(ALICE);
    if let Some(metadata) = session.metadata.as_object_mut() {
        metadata.remove(SLOT);
    }
    hub.service.sessions.save(&session).unwrap();

    hub.ask(ALICE, "again").await.unwrap();

    let second = &hub.claude.calls()[1];
    assert!(second.text().starts_with("[CONVERSATION SO FAR"));
    assert!(second.text().contains("hello"));
    assert!(!second.prompt.contains("CONVERSATION SO FAR"));
    assert!(!second.prompt.contains("default answer"));
}

#[tokio::test]
async fn shared_room_cli_turn_is_one_shot_and_speaker_scoped() {
    let hub = hub(claude_first).await;
    const BOB: &str = "0b7f3d52-8f0a-4c2f-9d44-3c1e6a1f4e22";
    hub.claude_login(ALICE);
    hub.claude_login(BOB);
    let ask = |user: &'static str, message: &'static str| {
        hub.service.process_message(
            "slack:T1:C123:1700.1",
            user,
            message,
            None,
            None,
            Some("C123"),
            Some("1700.1"),
            None,
            None,
            None,
            None,
            None,
            None,
        )
    };
    ask(ALICE, "alice asks").await.unwrap();
    let mut session = hub
        .service
        .sessions
        .get_or_create(
            "slack:T1:C123:1700.1",
            &crate::serve::session_owner_for_turn(
                "slack:T1:C123:1700.1",
                Some("C123"),
                Some("1700.1"),
                None,
                ALICE,
            ),
            hub.service.agent_id.as_str(),
        )
        .unwrap();
    assert!(session.is_shared_conversation());
    session.add_tool_message(
        "vault.get",
        &json!({}),
        &json!({ "secret": "alice-private-output" }),
        true,
        1,
        None,
    );
    let from = session.messages.len() - 1;
    session.attribute_messages_from(from, ALICE);
    hub.service.sessions.save(&session).unwrap();

    ask(BOB, "bob asks").await.unwrap();

    let calls = hub.claude.calls();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert!(
            call.has("--no-session-persistence"),
            "a shared room keeps no transcript"
        );
        assert!(!call.has("--resume") && !call.has("--session-id"));
    }
    let bob = calls[1].text();
    assert!(bob.contains("alice asks"), "{bob}");
    assert!(
        !bob.contains("alice-private-output"),
        "another speaker's tool output never reaches the CLI: {bob}"
    );
    let session = hub.service.sessions.get(&session.id).unwrap().unwrap();
    assert!(session.engine_slot(SLOT).is_none());
}

#[tokio::test]
async fn seeds_run_once_across_a_hop() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.chatgpt_login(ALICE).await;
    let probe = crate::mcp_turn::testing::Probe::named("probe.approved");
    hub.service.tools.register(probe.clone());
    hub.claude.reply(None, &rejected());

    let reply = workforce_agent::runner::CURRENT_PREEXECUTE
        .scope(
            workforce_agent::runner::PreExecutedTool {
                tool_name: "probe.approved".into(),
                tool_input: json!({}),
            },
            hub.ask(ALICE, "go ahead"),
        )
        .await
        .unwrap();

    assert_eq!(reply, SKY);
    assert_eq!(probe.runs(), 1, "the approved call ran once for the turn");
    let first = &hub.claude.calls()[0];
    assert!(
        first.text().contains("[CALLS MADE FOR THIS TURN"),
        "the CLI route was told of it: {}",
        first.text()
    );
}

#[tokio::test]
async fn mid_round_follow_up_runs_second_round_with_superseded_discard() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.claude.script(
        Some(1),
        &format!("sleep 1\n{}", print(&answer("First draft."))),
    );
    hub.claude.reply(Some(2), &answer("With staging too."));
    let (tx, rx) = mpsc::channel(8);
    let running = hub.claude.started(1);
    let follow_up = tokio::spawn(async move {
        running.await;
        tx.send(InjectedMessage::UserMessage {
            text: "also check staging".into(),
            user: ALICE.into(),
            external_id: None,
        })
        .await
        .unwrap();
        tx
    });

    let (events, result) = hub.stream(ALICE, "check prod", Some(rx), None).await;
    drop(follow_up.await);

    let result = result.unwrap();
    assert_eq!(result.response, "With staging too.");
    assert_eq!(result.injected_user_messages.len(), 1);
    let calls = hub.claude.calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].has("--resume"));
    assert!(calls[1].text().contains("also check staging"));
    assert!(events.iter().any(|e| matches!(
        e,
        ProgressEvent::StreamDiscard {
            reason: StreamDiscardReason::Superseded
        }
    )));
    let session = hub.session(ALICE);
    assert!(session
        .messages
        .iter()
        .any(|m| m.content == "also check staging"));
}

#[tokio::test]
async fn session_not_found_retry_keeps_the_rounds_follow_ups() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.claude.script(
        Some(1),
        &format!("sleep 1\n{}", print(&answer("First draft."))),
    );
    hub.claude.script(
        Some(2),
        "echo 'No conversation found with session ID' >&2\nexit 1\n",
    );
    hub.claude.reply(Some(3), &answer("With staging too."));
    let (tx, rx) = mpsc::channel(8);
    let running = hub.claude.started(1);
    let follow_up = tokio::spawn(async move {
        running.await;
        tx.send(InjectedMessage::UserMessage {
            text: "also check staging".into(),
            user: ALICE.into(),
            external_id: None,
        })
        .await
        .unwrap();
        tx
    });

    let (_, result) = hub.stream(ALICE, "check prod", Some(rx), None).await;
    drop(follow_up.await);

    assert_eq!(result.unwrap().response, "With staging too.");
    let calls = hub.claude.calls();
    assert_eq!(calls.len(), 3);
    assert!(!calls[2].has("--resume"));
    assert!(
        calls[2].text().contains("also check staging"),
        "the fresh transcript is given the follow-up it answers: {}",
        calls[2].text()
    );
}

#[tokio::test]
async fn steer_stop_cancels_cli_round() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.claude.script(
        Some(1),
        &format!(
            "{}sleep 60\n",
            print(&[assistant("msg-1", "Working on it.")])
        ),
    );
    hub.claude.reply(Some(2), &answer("Stopped as asked."));
    let (tx, rx) = mpsc::channel(8);
    let running = hub.claude.started(1);
    let stop = tokio::spawn(async move {
        running.await;
        tx.send(InjectedMessage::SteerStop {
            text: "stop".into(),
            user: ALICE.into(),
        })
        .await
        .unwrap();
        tx
    });

    let started = std::time::Instant::now();
    let (_, result) = hub.stream(ALICE, "do the long thing", Some(rx), None).await;
    drop(stop.await);

    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the round was cut short"
    );
    assert_eq!(result.unwrap().response, "Stopped as asked.");
    let calls = hub.claude.calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].text().contains("The user asked you to stop"));
    assert_eq!(calls[1].flag("--max-turns"), Some("1"));
}

#[tokio::test]
async fn stopped_round_usage_is_billed() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    let block = |kind: Value| {
        json!({
            "type": "assistant",
            "message": {
                "id": "msg-1",
                "role": "assistant",
                "content": [kind],
                "usage": {
                    "input_tokens": 7,
                    "output_tokens": 3,
                    "cache_creation_input_tokens": 2,
                    "cache_read_input_tokens": 40,
                },
            },
        })
    };
    hub.claude.script(
        Some(1),
        &format!(
            "{}sleep 60\n",
            print(&[
                block(json!({ "type": "text", "text": "Working on it." })),
                block(
                    json!({ "type": "tool_use", "name": "mcp__workforce__web_fetch", "input": {} })
                ),
            ])
        ),
    );
    hub.claude.reply(Some(2), &answer("Stopped as asked."));
    let (tx, rx) = mpsc::channel(8);
    let running = hub.claude.started(1);
    let stop = tokio::spawn(async move {
        running.await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        tx.send(InjectedMessage::SteerStop {
            text: "stop".into(),
            user: ALICE.into(),
        })
        .await
        .unwrap();
        tx
    });

    let (_, result) = hub.stream(ALICE, "do the long thing", Some(rx), None).await;
    drop(stop.await);

    let usage = result.unwrap().total_usage;
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens
        ),
        (17, 8, 2, 140),
        "the stopped round's model call once, then the wrap-up's"
    );
}

#[tokio::test]
async fn stopped_turn_whose_wrap_up_fails_is_final_not_handed_over() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.chatgpt_login(ALICE).await;
    hub.claude.script(
        Some(1),
        &format!(
            "{}sleep 60\n",
            print(&[assistant("msg-1", "Working on it.")])
        ),
    );
    hub.claude.script(Some(2), "exit 1\n");
    let (tx, rx) = mpsc::channel(8);
    let running = hub.claude.started(1);
    let stop = tokio::spawn(async move {
        running.await;
        tx.send(InjectedMessage::SteerStop {
            text: "stop".into(),
            user: ALICE.into(),
        })
        .await
        .unwrap();
        tx
    });

    let (_, result) = hub.stream(ALICE, "do the long thing", Some(rx), None).await;
    drop(stop.await);

    assert!(result.is_err(), "the stopped turn ends on its own route");
    assert_eq!(hub.claude.calls().len(), 2);
    assert_eq!(
        hub.chatgpt_requests().await,
        0,
        "a turn the person stopped is never rerun on the next route"
    );
}

#[tokio::test]
async fn follow_up_round_failure_after_an_answer_is_final_not_handed_over() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.chatgpt_login(ALICE).await;
    hub.claude.script(
        Some(1),
        &format!("sleep 1\n{}", print(&answer("First draft."))),
    );
    hub.claude.script(Some(2), "exit 1\n");
    let (tx, rx) = mpsc::channel(8);
    let running = hub.claude.started(1);
    let follow_up = tokio::spawn(async move {
        running.await;
        tx.send(InjectedMessage::UserMessage {
            text: "also check staging".into(),
            user: ALICE.into(),
            external_id: None,
        })
        .await
        .unwrap();
        tx
    });

    let (_, result) = hub.stream(ALICE, "check prod", Some(rx), None).await;
    drop(follow_up.await);

    assert!(result.is_err(), "the answered turn ends on its own route");
    assert_eq!(hub.claude.calls().len(), 2);
    assert_eq!(
        hub.chatgpt_requests().await,
        0,
        "a turn whose answer was already streamed is never answered again"
    );
}

#[tokio::test]
async fn cancel_kills_cli_and_ends_turn_record() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.claude.script(None, "sleep 60\n");
    let cancel = tokio_util::sync::CancellationToken::new();
    let stopping = cancel.clone();
    let running = hub.claude.started(1);
    tokio::spawn(async move {
        running.await;
        stopping.cancel();
    });

    let started = std::time::Instant::now();
    let (_, result) = hub.stream(ALICE, "long", None, Some(cancel)).await;

    assert!(result.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the CLI was killed"
    );
    let session = hub.session(ALICE);
    assert!(
        !hub.service
            .sessions
            .interrupted_turns()
            .unwrap()
            .iter()
            .any(|turn| turn.session_id == session.id),
        "a user stop ends the turn record"
    );
}

#[test]
fn mcp_tool_timeout_sits_below_turn_deadline() {
    let turn = cli::client_timeout(AGENT_TURN_TIMEOUT);
    assert!(turn.client < AGENT_TURN_TIMEOUT);
    assert_eq!(AGENT_TURN_TIMEOUT - turn.client, ANSWER_RESERVE);
    assert!(turn.client >= crate::mcp_turn::ClientTimeout::for_tool(turn.tool).client);
    let review = cli::client_timeout(Duration::from_secs(90));
    assert!(review.client > Duration::ZERO && review.client < Duration::from_secs(90));
}

#[tokio::test]
async fn mcp_tool_timeout_reaches_the_cli() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);

    hub.ask(ALICE, "hello").await.unwrap();

    let sent: u128 = hub.claude.calls()[0].tool_timeout.parse().unwrap();
    assert_eq!(
        sent,
        cli::client_timeout(AGENT_TURN_TIMEOUT).client.as_millis()
    );
}

#[tokio::test]
async fn claude_effort_none_asks_the_least() {
    let off = hub(|config| {
        claude_first(config);
        config.llm.thinking_budget_tokens = Some(0);
    })
    .await;
    off.claude_login(ALICE);
    off.ask(ALICE, "hello").await.unwrap();
    assert_eq!(
        off.claude.calls()[0].flag("--effort"),
        Some("low"),
        "thinking off asks the least the CLI takes, as a completion does"
    );

    let medium = hub(|config| {
        claude_first(config);
        config.llm.thinking_budget_tokens = Some(8000);
    })
    .await;
    medium.claude_login(ALICE);
    medium.ask(ALICE, "hello").await.unwrap();
    assert_eq!(medium.claude.calls()[0].flag("--effort"), Some("medium"));
}

/// `hub` with its live `[llm] effort` set to `effort`, as a config edit
/// would leave it.
fn set_effort(hub: &Hub, effort: Option<&str>) {
    let mut resolved = (*hub.service.resolved.snapshot()).clone();
    resolved.effort = effort.map(String::from);
    hub.service.resolved.store(resolved);
}

#[tokio::test]
async fn the_configured_effort_reaches_the_cli_and_a_pinned_one_beats_it() {
    let hub = hub(|config| {
        claude_first(config);
        config.llm.thinking_budget_tokens = Some(1_000);
    })
    .await;
    hub.claude_login(ALICE);

    set_effort(&hub, Some("max"));
    hub.ask(ALICE, "one").await.unwrap();
    assert_eq!(
        hub.claude.calls()[0].flag("--effort"),
        Some("max"),
        "the agent's effort beats its thinking budget"
    );

    set_effort(&hub, Some("xhigh"));
    hub.ask(ALICE, "two").await.unwrap();
    assert_eq!(
        hub.claude.calls()[1].flag("--effort"),
        Some("high"),
        "a level the CLI lacks asks for the one below it"
    );

    let mut session = hub.session(ALICE);
    session.set_claude_chat_choice(None, Some("low"));
    hub.service.sessions.save(&session).unwrap();
    hub.ask(ALICE, "three").await.unwrap();
    assert_eq!(
        hub.claude.calls()[2].flag("--effort"),
        Some("low"),
        "the session's own choice wins"
    );

    session.set_claude_chat_choice(None, None);
    hub.service.sessions.save(&session).unwrap();
    set_effort(&hub, None);
    hub.ask(ALICE, "four").await.unwrap();
    assert_eq!(
        hub.claude.calls()[3].flag("--effort"),
        Some("low"),
        "with no effort the budget decides again"
    );
}

/// The `reasoning.effort` of each request the ChatGPT backend received.
async fn chatgpt_efforts(hub: &Hub) -> Vec<serde_json::Value> {
    hub.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/responses")
        .map(|r| {
            serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["reasoning"]["effort"]
                .clone()
        })
        .collect()
}

#[tokio::test]
async fn the_configured_effort_reaches_the_chatgpt_request_on_every_turn_path() {
    let hub = hub(|_| {}).await;
    hub.chatgpt_login(ALICE).await;

    set_effort(&hub, Some("high"));
    hub.ask(ALICE, "one").await.unwrap();
    set_effort(&hub, Some("xhigh"));
    let (_, streamed) = hub.stream(ALICE, "two", None, None).await;
    streamed.unwrap();
    set_effort(&hub, None);
    hub.ask(ALICE, "three").await.unwrap();

    assert_eq!(
        chatgpt_efforts(&hub).await,
        [json!("high"), json!("xhigh"), serde_json::Value::Null],
        "the live effort reaches the backend, streamed or not, and none leaves its default"
    );
}

#[tokio::test]
async fn advisor_model_backend_on_claude_code_primary_runs_on_the_cli() {
    let hub = hub(claude_first).await;
    let token = hub.claude_login(ALICE);
    hub.claude.reply(None, &answer("The plan holds."));
    let config = workforce_common::settings::AdvisorConfig {
        enabled: true,
        source: workforce_common::settings::AdvisorSource::Model,
        ..Default::default()
    };
    let request = crate::advisor::AdvisorRequest {
        payload: workforce_common::settings::AdvisorPayload::Transcript,
        payload_text: "user: ship it?".into(),
        payload_truncated: false,
        question: Some("Is the plan sound?".into()),
        lenses: Vec::new(),
    };

    let outcome = workforce_agent::runner::CURRENT_USER_ID
        .scope(
            ALICE.to_string(),
            crate::advisor::dispatch(
                &hub.service,
                &config,
                &request,
                Some(ALICE),
                &std::sync::Arc::default(),
                None,
            ),
        )
        .await;

    match outcome {
        crate::advisor::AdvisorOutcome::Advice(advice) => {
            assert_eq!(advice.text, "The plan holds.")
        }
        crate::advisor::AdvisorOutcome::Unavailable(failure) => {
            panic!("the review ran on the agent's CLI route: {failure:?}")
        }
    }
    let calls = hub.claude.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].token, token);
    assert_eq!(hub.chatgpt_requests().await, 0);
}

#[tokio::test]
async fn model_coding_engine_on_claude_code_primary_runs_on_the_cli() {
    use crate::coding_engine::CodingEngine;
    let hub = hub(claude_first).await;
    let token = hub.claude_login(ALICE);
    hub.claude.reply(None, &answer("Fixed the typo."));
    let workdir = hub.dir.path().join("checkout");
    std::fs::create_dir_all(&workdir).unwrap();
    let sandbox = crate::coding_sandbox::adopt_workdir(workdir, Vec::new(), None);
    let round = crate::coding_engine::CodingRound::one_shot(
        "Fix the typo in README.md".into(),
        Duration::from_secs(600),
    );
    let ctx = crate::coding_engine::RoundContext {
        service: &hub.service,
        identity: Some(ALICE),
        stand_in: workforce_common::subscription::SCHEDULER,
        ticket_id: None,
        description: "coding.session",
        resume: crate::mcp_gate::RouteResume::Nowhere,
    };

    let result = crate::coding_engine::AgentLoopEngine
        .run(sandbox, &round, &ctx, None, None)
        .await
        .unwrap();

    assert_eq!(result.text, "Fixed the typo.");
    assert_eq!(result.completion, crate::claude_code::Completion::Success);
    let calls = hub.claude.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].token, token);
    assert!(
        calls[0]
            .flag("--allowedTools")
            .is_some_and(|tools| tools.contains("mcp__workforce__Bash")),
        "the sandbox's own tools are served to the CLI"
    );
    assert_eq!(hub.chatgpt_requests().await, 0);
}

#[tokio::test]
async fn model_engine_hub_tool_is_gated_once_on_a_cli_route() {
    use crate::coding_engine::CodingEngine;
    use workforce_agent::tools::approval::testing::{AskEveryCall, RecordingTool};
    use workforce_agent::tools::approval::TurnApprovals;
    use workforce_core::policy::PermissionChoice;
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    let tool = Arc::new(RecordingTool::default());
    hub.service
        .tools
        .register(Arc::clone(&tool) as Arc<dyn workforce_agent::tools::Tool>);
    hub.service.set_permission_gate(Arc::new(AskEveryCall));
    let call = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "catalogue_remove_entry", "arguments": { "name": "figma" } },
    });
    hub.claude.script(
        None,
        &format!(
            r#"cfg=$(awk 'prev=="--mcp-config"{{print}} {{prev=$0}}' "$dir/$n.args")
url=$(sed -n 's/.*"url": *"\([^"]*\)".*/\1/p' "$cfg")
auth=$(sed -n 's/.*"Authorization": *"\([^"]*\)".*/\1/p' "$cfg")
curl -s -X POST "$url" -H "Authorization: $auth" -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' --data '{call}' > "$dir/$n.mcp"
{answer}"#,
            answer = print(&answer("Removed it."))
        ),
    );
    let workdir = hub.dir.path().join("checkout");
    std::fs::create_dir_all(&workdir).unwrap();
    let sandbox =
        crate::coding_sandbox::adopt_workdir(workdir, vec!["catalogue.*".to_string()], None);
    let round = crate::coding_engine::CodingRound::one_shot(
        "Remove the figma entry".into(),
        Duration::from_secs(600),
    );
    let ctx = crate::coding_engine::RoundContext {
        service: &hub.service,
        identity: Some(ALICE),
        stand_in: workforce_common::subscription::SCHEDULER,
        ticket_id: None,
        description: "coding.session",
        resume: crate::mcp_gate::RouteResume::Nowhere,
    };
    let (progress, mut events) = mpsc::channel(64);
    let ledger = Arc::new(TurnApprovals::default());

    let run = workforce_agent::runner::scope_turn(
        Some(progress),
        Some(Arc::clone(&ledger)),
        crate::coding_engine::AgentLoopEngine.run(sandbox, &round, &ctx, None, None),
    );
    let approve = async {
        let mut prompts = 0;
        while let Some(event) = events.recv().await {
            if let ProgressEvent::ApprovalRequired { request_id, .. } = event {
                prompts += 1;
                assert!(ledger.answer(&request_id, PermissionChoice::AllowOnce));
            }
        }
        prompts
    };
    let (result, prompts) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(run, approve)
    })
    .await
    .expect("the held call was answered and the round finished");

    assert_eq!(result.unwrap().text, "Removed it.");
    assert_eq!(
        prompts,
        1,
        "one gate, one prompt: {}",
        hub.claude.file("1.mcp")
    );
    assert_eq!(
        tool.runs(),
        [json!({ "name": "figma" })],
        "held for the person, then run once: {}",
        hub.claude.file("1.mcp")
    );
}

#[tokio::test]
async fn run_isolated_on_cli_route_never_runs_runner_loop() {
    let hub = hub(|_| {}).await;
    hub.claude_login(ALICE);
    hub.chatgpt_login(ALICE).await;
    hub.claude.reply(None, &answer("Isolated on Claude."));

    let run = crate::serve::IsolatedRun {
        principal: workforce_common::session::TurnPrincipal {
            run_as: Some(ALICE.into()),
            bill_to: None,
        },
        route: crate::serve::IsolatedRoute::Pinned("claude-code:sonnet".into()),
    };
    let result = workforce_agent::runner::CURRENT_USER_ID
        .scope(
            ALICE.to_string(),
            hub.service.run_isolated(
                "subagent",
                "summarise",
                None,
                None,
                &run,
                crate::job_runner::AgentProfile::Worker,
                None,
                None,
            ),
        )
        .await
        .unwrap();

    assert_eq!(result.response, "Isolated on Claude.");
    assert_eq!(hub.chatgpt_requests().await, 0, "no runner loop ran");
    let call = &hub.claude.calls()[0];
    assert!(
        call.has("--no-session-persistence"),
        "an isolated run is one-shot"
    );
}

#[tokio::test]
async fn grounding_on_claude_code_spec_uses_chain_loop() {
    let hub = hub(|config| {
        config.llm.failover = vec!["claude-code:sonnet".into()];
    })
    .await;
    hub.claude_login(ALICE);
    hub.claude.reply(None, &answer("[]"));

    let tool_loop =
        crate::agent_turn::tool_loop::tool_loop(&hub.service, Some("claude-code:sonnet")).await;
    let outcome = workforce_agent::runner::CURRENT_USER_ID
        .scope(
            ALICE.to_string(),
            tool_loop.run(
                crate::advisor_loop::AdvisorLoopRequest {
                    system_prompt: "Verify the claims.".into(),
                    user_message: "ANSWER TO VERIFY: 2 + 2 = 4".into(),
                    tools: Vec::new(),
                    model: None,
                    max_rounds: 4,
                    budget: None,
                    cancel: None,
                    label: "claims",
                },
                &|_, _, _| {},
            ),
        )
        .await;

    match outcome {
        crate::advisor_loop::AdvisorLoopOutcome::Success { text, model, .. } => {
            assert_eq!(text, "[]");
            assert_eq!(model, "claude-code:sonnet");
        }
        _ => panic!("the CLI answered the loop"),
    }
    let call = &hub.claude.calls()[0];
    assert!(call.prompt.starts_with("Verify the claims."));
    assert_eq!(call.flag("--max-turns"), Some("4"));
    assert_eq!(hub.chatgpt_requests().await, 0);
}

#[tokio::test]
async fn automated_turn_without_login_returns_turn_unavailable() {
    let hub = hub(|config| {
        claude_first(config);
        config.llm.failover = Vec::new();
    })
    .await;

    let err = hub.ask("scheduler", "nightly digest").await.unwrap_err();

    let unavailable = err
        .downcast_ref::<TurnUnavailable>()
        .unwrap_or_else(|| panic!("a named failure, not {err:#}"));
    assert_eq!(unavailable.misses.len(), 1);
    assert!(unavailable.misses[0].route.contains("claude-code:sonnet"));
    assert!(err.to_string().contains("no Claude login"), "{err}");
    assert!(hub.claude.calls().is_empty());
}

#[tokio::test]
async fn a_chain_that_failed_for_more_than_a_login_names_the_failure() {
    use wiremock::matchers::{method, path};
    let hub = hub(|config| {
        config.llm.failover = vec!["claude-code:sonnet".into()];
    })
    .await;
    hub.chatgpt_login(ALICE).await;
    wiremock::Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(wiremock::ResponseTemplate::new(400).set_body_json(json!({
            "error": { "message": "Invalid 'input': malformed item." }
        })))
        .with_priority(1)
        .mount(&hub.server)
        .await;

    let (events, result) = hub.stream(ALICE, "hello", None, None).await;

    let err = result.expect_err("no route served the turn");
    let unavailable = err
        .downcast_ref::<TurnUnavailable>()
        .unwrap_or_else(|| panic!("a named failure, not {err:#}"));
    assert_eq!(unavailable.misses.len(), 2, "{err}");
    assert!(err.to_string().contains("malformed item"), "{err}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ProgressEvent::ConnectRequired { .. })),
        "connecting Claude would not fix the ChatGPT route's failure"
    );
}

#[tokio::test]
async fn a_chatgpt_refusal_is_told_before_asking_for_claude() {
    use wiremock::matchers::{method, path};
    let hub = hub(|config| {
        config.llm.failover = vec!["claude-code:sonnet".into()];
    })
    .await;
    hub.chatgpt_login(ALICE).await;
    // The person's ChatGPT login is fine; the plan refuses the model.
    wiremock::Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(wiremock::ResponseTemplate::new(400).set_body_json(json!({
            "error": { "message": "The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account." }
        })))
        .with_priority(1)
        .mount(&hub.server)
        .await;

    let (events, result) = hub.stream(ALICE, "hello", None, None).await;

    let notice = result.unwrap().response;
    assert!(
        notice.starts_with("Your ChatGPT plan couldn't serve this: "),
        "{notice}"
    );
    assert!(
        notice.contains("not supported when using Codex"),
        "{notice}"
    );
    assert!(!notice.contains("subscription unavailable"), "{notice}");
    assert!(
        notice.contains("I can't answer on"),
        "the Claude connect steps follow the reason: {notice}"
    );
    assert!(notice.contains("claude setup-token"), "{notice}");
    assert!(
        !notice.contains("send it to me"),
        "no paste route when no turn can run to save it: {notice}"
    );
    assert_eq!(connect_prompt(&events).kind, SubscriptionKind::Claude);
    assert!(hub.claude.calls().is_empty());
}

#[test]
fn a_thinking_budget_asks_each_cli_one_effort() {
    for cli in [CliKind::ClaudeCode, CliKind::Codex] {
        assert_eq!(cli.effort(0), Some("low"), "thinking off asks the least");
        assert_eq!(cli.effort(1_000), Some("low"));
        assert_eq!(cli.effort(4_000), Some("medium"));
        assert_eq!(cli.effort(32_000), Some("high"));
    }
}

#[tokio::test]
async fn claude_only_chain_gives_setup_token_notice() {
    let hub = hub(|config| {
        claude_first(config);
        config.llm.failover = Vec::new();
    })
    .await;

    let (events, result) = hub.stream(ALICE, "hello", None, None).await;

    let notice = result.unwrap().response;
    assert!(notice.contains("claude setup-token"), "{notice}");
    let prompt = events
        .iter()
        .find_map(|e| match e {
            ProgressEvent::ConnectRequired { prompt } => Some(prompt.clone()),
            _ => None,
        })
        .expect("a connect prompt");
    assert_eq!(prompt.kind, SubscriptionKind::Claude);
    assert!(prompt.private);
    assert!(!prompt.resumes, "nothing re-runs a Claude notice");
    assert!(!prompt.reason.contains("setup-token"));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                ProgressEvent::Response { .. } | ProgressEvent::Done { .. }
            ))
            .count(),
        2
    );
    assert_eq!(hub.session(ALICE).messages.last().unwrap().content, notice);
    assert!(hub.claude.calls().is_empty());
    let _ = &hub.dir;
}

#[tokio::test]
async fn runner_only_chain_uses_agent_llm_and_keeps_hub_default() {
    use wiremock::matchers::{body_partial_json, method, path};
    let hub = hub(|_| {}).await;
    hub.chatgpt_login(ALICE).await;
    wiremock::Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_partial_json(json!({ "model": "gpt-5.2" })))
        .respond_with(wiremock::ResponseTemplate::new(400).set_body_json(json!({
            "detail": "The 'gpt-5.2' model is not supported when using Codex with a ChatGPT account."
        })))
        .with_priority(1)
        .mount(&hub.server)
        .await;
    assert!(
        crate::turn_loop::turn_chain(&hub.service, "chatgpt:gpt-5.2")
            .await
            .runner_only()
    );

    let reply = hub
        .service
        .process_message(
            "web",
            ALICE,
            "why is the sky blue?",
            None,
            None,
            None,
            None,
            None,
            None,
            Some("chatgpt:gpt-5.2"),
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        reply, SKY,
        "a runner-only turn keeps the provider chain's last resort, the hub default"
    );
    let served: Vec<Value> = hub
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
        .map(|body| body["model"].clone())
        .collect();
    assert_eq!(served.last(), Some(&json!("gpt-6.1-sol")));
    assert!(hub.claude.calls().is_empty());
}

#[tokio::test]
async fn engine_slot_survives_lost_cas() {
    let hub = hub(claude_first).await;
    let stored = hub.session(ALICE);
    hub.service.sessions.save(&stored).unwrap();
    let mut turn = stored.clone();
    let slot = EngineSlot {
        synced_through: Some("m-1".into()),
        ..EngineSlot::fresh()
    };
    turn.set_engine_slot(SLOT, &slot);

    crate::serve::persist_turn_owned_metadata(&hub.service.sessions, &turn);

    assert_eq!(
        hub.session(ALICE).engine_slot(SLOT),
        Some(slot),
        "the slot is written even when the turn's own save was lost"
    );
}

/// A workspace on the remote worker whose scope runs durable jobs.
struct DurableScope(PathBuf);

#[async_trait::async_trait]
impl crate::coding_sandbox::CodingSandbox for DurableScope {
    fn workdir(&self) -> &std::path::Path {
        &self.0
    }
    fn setup(&self) -> &[crate::projects::setup::SetupReport] {
        &[]
    }
    fn hub_tool_patterns(&self) -> Vec<String> {
        Vec::new()
    }
    fn fresh(&self) -> bool {
        true
    }
    async fn exec(
        &self,
        _: crate::coding_sandbox::SandboxCommand,
    ) -> eyre::Result<crate::coding_sandbox::ExecOutcome> {
        unreachable!()
    }
    async fn read(&self, _: &std::path::Path, _: usize) -> eyre::Result<Option<String>> {
        unreachable!()
    }
    async fn write(&self, _: &std::path::Path, _: &str) -> eyre::Result<()> {
        unreachable!()
    }
    async fn list(
        &self,
        _: &std::path::Path,
    ) -> eyre::Result<Vec<crate::coding_sandbox::DirEntry>> {
        unreachable!()
    }
    fn placement(&self) -> Option<crate::claude_code::RemotePlacement> {
        Some(crate::claude_code::RemotePlacement {
            endpoint: workforce_agent::tools::exec_runner::RunnerEndpoint::Unix(
                "/nonexistent/runner.sock".into(),
            ),
            scope: workforce_agent::tools::exec_runner::ScopeId::new("at-scope"),
            jobs: true,
        })
    }
}

#[tokio::test]
async fn agent_turn_remote_placement_is_connection_bound() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    let login = workforce_agent::runner::CURRENT_USER_ID
        .sync_scope(ALICE.to_string(), || {
            hub.service.logins.resolve(SubscriptionKind::Claude)
        })
        .unwrap();
    let turn = cli_turn(Vec::new(), &hub.service);
    let sandbox: Arc<dyn crate::coding_sandbox::CodingSandbox> =
        Arc::new(DurableScope("/scope/work".into()));
    let transcript = conversation::Transcript::fresh();
    let round = round::Round {
        service: &hub.service,
        turn: &turn,
        model: "sonnet",
        login: &login,
        sandbox: &sandbox,
        instructions: &cli::Instructions::default(),
        tools: None,
        transcript: &transcript,
        pinned_effort: None,
        deadline: tokio::time::Instant::now() + AGENT_TURN_TIMEOUT,
        program: None,
    };

    let config = round.config(
        8,
        vec![json!({"type": "text", "text": "hi"})],
        std::path::Path::new(".prompt.md"),
    );

    let placement = config.remote.expect("the turn runs on the worker");
    assert!(
        !placement.jobs,
        "a turn is not a durable job: it ends with the connection, and the sweep reruns it"
    );
    assert!(config.jobs.is_none());
    assert_eq!(
        config.contained.unwrap().system_prompt_file,
        std::path::Path::new("/scope/work/.prompt.md")
    );
}

/// The system prompt files under `dir`, wherever a workspace put them.
fn prompt_files(dir: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|entry| {
            let path = entry.path();
            match path.is_dir() {
                true => prompt_files(&path),
                false => path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| name.starts_with(".workforce-system-prompt-"))
                    .map(|_| vec![path.clone()])
                    .unwrap_or_default(),
            }
        })
        .collect()
}

#[tokio::test]
async fn abandoned_turn_removes_its_private_system_prompt() {
    use std::os::unix::fs::PermissionsExt;
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.claude.script(None, "sleep 60\n");
    let rig = hub.service.turn_rig.get::<AgentTurnRig>().unwrap();
    let run = Box::pin(workforce_agent::runner::CURRENT_USER_ID.scope(
        ALICE.to_string(),
        rig.run(
            &hub.service,
            cli_turn(vec![ContentBlock::Text { text: "hi".into() }], &hub.service),
            None,
            None,
        ),
    ));
    let mut run = run;
    tokio::select! {
        _ = &mut run => panic!("the round runs until it is abandoned"),
        () = async {
            while hub.claude.calls().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        } => {}
    }
    let written = prompt_files(hub.dir.path());
    assert_eq!(written.len(), 1, "{written:?}");
    assert_eq!(
        std::fs::metadata(&written[0]).unwrap().permissions().mode() & 0o777,
        0o600,
        "only the CLI's own user reads the agent's prompt"
    );

    drop(run);

    tokio::time::timeout(Duration::from_secs(10), async {
        while written[0].exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("an abandoned turn leaves no system prompt behind");
}

#[tokio::test]
async fn full_turn_pool_is_waited_on_only_until_the_turn_deadline() {
    let hub = hub(|config| {
        claude_first(config);
        config.subscription.max_concurrent_cli_turns = 1;
    })
    .await;
    hub.claude_login(ALICE);
    let _held = hub
        .service
        .subscriptions()
        .pools()
        .turns()
        .acquire(None)
        .await
        .unwrap();
    let rig = hub.service.turn_rig.get::<AgentTurnRig>().unwrap();
    let turn = CliTurn {
        timeout: Duration::from_millis(500),
        ..cli_turn(vec![ContentBlock::Text { text: "hi".into() }], &hub.service)
    };

    let attempt = tokio::time::timeout(
        Duration::from_secs(10),
        workforce_agent::runner::CURRENT_USER_ID
            .scope(ALICE.to_string(), rig.run(&hub.service, turn, None, None)),
    )
    .await
    .expect("a turn with no cancel still stops waiting at its deadline")
    .unwrap();

    assert!(
        matches!(attempt, CliAttempt::Unserved(_)),
        "a turn that never got a slot produced nothing, so the next route may take it"
    );
    assert!(hub.claude.calls().is_empty());
}

#[tokio::test]
async fn document_reaches_cli_turn_as_text() {
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    let rig = hub.service.turn_rig.get::<AgentTurnRig>().unwrap();
    let document = ContentBlock::Document {
        source: ImageSource {
            source_type: "base64".into(),
            media_type: "application/pdf".into(),
            data: "AAAA".into(),
        },
    };

    let attempt = workforce_agent::runner::CURRENT_USER_ID
        .scope(
            ALICE.to_string(),
            rig.run(
                &hub.service,
                cli_turn(vec![document], &hub.service),
                None,
                None,
            ),
        )
        .await
        .unwrap();

    assert!(
        matches!(attempt, CliAttempt::Served(_)),
        "a document never makes the route unservable"
    );
    let content = &hub.claude.calls()[0].stdin["message"]["content"];
    assert!(content
        .as_array()
        .unwrap()
        .iter()
        .all(|block| block["type"] == "text"));
    assert!(hub.claude.calls()[0].text().contains("PDF `attachment`"));
}

/// The code the mock auth server issues.
const CODE: &str = "WXYZ-9876";

async fn issues_codes(hub: &Hub) {
    use wiremock::matchers::{method, path};
    wiremock::Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "device_auth_id": "dev-1",
            "user_code": CODE,
            "interval": "5",
        })))
        .mount(&hub.server)
        .await;
}

/// The auth server grants the sign-in; the backend answers its token.
async fn grants_sign_in(hub: &Hub) {
    use wiremock::matchers::{body_string_contains, method, path};
    let access = crate::subscription::test_support::access("signed-in");
    wiremock::Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "authorization_code": "auth-code-1",
            "code_challenge": "challenge-1",
            "code_verifier": "verifier-1",
        })))
        .mount(&hub.server)
        .await;
    wiremock::Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id_token": crate::subscription::test_support::jwt(json!({
                "sub": "user-1",
                "email": "alice@example.com",
                "https://api.openai.com/auth": {"chatgpt_account_id": format!("acct-{}", uuid::Uuid::new_v4())},
            })),
            "access_token": access,
            "refresh_token": "rt-signed-in",
        })))
        .mount(&hub.server)
        .await;
    answer_bearer(&hub.server, &access).await;
}

fn alice() -> uuid::Uuid {
    uuid::Uuid::parse_str(ALICE).unwrap()
}

fn connect_prompt(events: &[ProgressEvent]) -> workforce_common::subscription::ConnectPrompt {
    events
        .iter()
        .find_map(|e| match e {
            ProgressEvent::ConnectRequired { prompt } => Some(prompt.clone()),
            _ => None,
        })
        .expect("a connect prompt")
}

/// A Slack thread in workspace `T2`, and a channel with no threads in `T1`.
const THREAD: (&str, Option<&str>, Option<&str>) =
    ("slack:T2:C9:1700.1", Some("C9"), Some("1700.1"));
const UNTHREADED: (&str, Option<&str>, Option<&str>) = ("slack:T1:C1", Some("C1"), None);

impl Hub {
    fn device(&self) -> crate::subscription::DeviceLogins {
        self.service.subscriptions().device_logins().clone()
    }

    /// The registry the hub's sign-ins complete against, wired as the hub
    /// wires it, with `surfaces` as the Slack workspaces.
    fn wired(
        &self,
        surfaces: crate::slack_manager::WorkspaceSurfaces,
    ) -> Arc<crate::agent_registry::AgentRegistry> {
        let registry = Arc::new(crate::agent_registry::AgentRegistry::single(Arc::clone(
            &self.service,
        )));
        let (broadcast, _) = tokio::sync::broadcast::channel(16);
        let stop = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        connect::wire(
            &registry,
            surfaces,
            crate::session_push::SessionPush::new(None, broadcast),
            stop,
        );
        registry
    }

    fn answering(&self) -> Arc<crate::agent_registry::AgentRegistry> {
        self.wired(Default::default())
    }

    fn connected_reruns(&self) -> usize {
        self.session(ALICE)
            .messages
            .iter()
            .filter(|m| m.content == connect::CONNECTED_PROMPT)
            .count()
    }

    /// Wait for the session's latest answer to be `text`.
    async fn answered_with(&self, text: &str) {
        for _ in 0..200 {
            let last = self
                .session(ALICE)
                .messages
                .last()
                .map(|m| m.content.clone());
            if last.as_deref() == Some(text) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "never answered with {text:?}: {:?}",
            self.session(ALICE).messages.last()
        );
    }

    /// Hand the code of the sign-in `prompt` started to Alice through
    /// `surface`, as a Slack turn in `workspace` would.
    async fn deliver(
        &self,
        surface: &Arc<Sent>,
        workspace: &str,
        (channel, thread): (&str, Option<&str>),
        session_id: &str,
        prompt: &workforce_common::subscription::ConnectPrompt,
    ) {
        let surface: Arc<dyn workforce_common::channels::ChannelSurface> = surface.clone();
        crate::slack_connect::deliver_code(
            &self.service,
            &surface,
            workspace,
            (channel, thread),
            crate::slack_connect::Requester {
                slack_id: "U-ALICE",
                identity: Some(ALICE),
            },
            session_id,
            prompt,
        )
        .await;
    }

    /// Alice's session on `channel`.
    fn slack_session(
        &self,
        (channel, channel_id, thread): (&str, Option<&str>, Option<&str>),
    ) -> workforce_common::session::Session {
        self.service
            .sessions
            .get_or_create(
                channel,
                &crate::serve::session_owner_for_turn(channel, channel_id, thread, None, ALICE),
                self.service.agent_id.as_str(),
            )
            .unwrap()
    }
}

#[tokio::test]
async fn interactive_turn_without_chatgpt_login_starts_flow_code_not_in_transcript() {
    let hub = hub(|_| {}).await;
    issues_codes(&hub).await;

    let (events, result) = hub.stream(ALICE, "why is the sky blue?", None, None).await;

    let result = result.unwrap();
    assert_eq!(
        crate::subscription::route_line(&hub.service, &result.model, None),
        None,
        "a notice is no turn a model ran"
    );
    let notice = result.response;
    assert!(notice.contains("ChatGPT"), "{notice}");
    let prompt = connect_prompt(&events);
    assert_eq!(prompt.kind, SubscriptionKind::ChatGpt);
    assert!(prompt.private);
    assert!(prompt.resumes, "the message waits on the sign-in");
    let flow = hub
        .device()
        .get(alice(), prompt.flow_id.expect("a sign-in was started"))
        .unwrap();
    assert_eq!(flow.user_code, CODE);
    assert_eq!(
        flow.target,
        crate::subscription::LoginTarget::AgentUser {
            agent_id: hub.service.agent_id.to_string()
        }
    );
    let waiter: connect::ConnectWaiter = serde_json::from_value(flow.waiters[0].clone()).unwrap();
    let session = hub.session(ALICE);
    let asked = session
        .messages
        .iter()
        .find(|m| m.content == "why is the sky blue?")
        .unwrap();
    assert_eq!(waiter.message_id, asked.id);
    assert_eq!(waiter.session_id, session.id);
    assert!(!format!("{events:?}").contains(CODE), "no code in progress");
    assert!(!notice.contains(CODE));
    assert!(session.messages.iter().all(|m| !m.content.contains(CODE)));
    assert_eq!(session.messages.last().unwrap().content, notice);
    assert_eq!(hub.chatgpt_requests().await, 0);
}

#[tokio::test]
async fn connect_prompt_broadcast_carries_no_user_code() {
    let hub = hub(|_| {}).await;
    issues_codes(&hub).await;

    let (events, _) = hub.stream(ALICE, "why is the sky blue?", None, None).await;

    let prompt = connect_prompt(&events);
    let flow = prompt.flow_id.expect("a sign-in was started").to_string();
    for wire in [
        serde_json::to_string(&crate::broadcast::BroadcastMessage::connect_prompt(
            "s-1",
            prompt.clone(),
        ))
        .unwrap(),
        serde_json::to_string(&workforce_nats::chat::ChatProgress::ConnectPrompt { prompt })
            .unwrap(),
    ] {
        assert!(wire.contains(&flow), "{wire}");
        assert!(wire.contains("\"private\":true"), "{wire}");
        assert!(!wire.contains(CODE), "{wire}");
    }
}

/// How many codes the auth server issued.
async fn codes_issued(hub: &Hub) -> usize {
    hub.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api/accounts/deviceauth/usercode")
        .count()
}

#[tokio::test]
async fn second_message_joins_live_flow() {
    let hub = hub(|_| {}).await;
    issues_codes(&hub).await;

    let (first, _) = hub.stream(ALICE, "first", None, None).await;
    let (second, _) = hub.stream(ALICE, "second", None, None).await;

    let flow_id = connect_prompt(&first).flow_id.unwrap();
    assert_eq!(connect_prompt(&second).flow_id, Some(flow_id));
    assert_eq!(hub.device().get(alice(), flow_id).unwrap().waiters.len(), 2);
    assert_eq!(codes_issued(&hub).await, 1);
}

#[tokio::test]
async fn chat_turn_joins_my_settings_global_flow_instead_of_replacing() {
    let hub = hub(|_| {}).await;
    issues_codes(&hub).await;
    let heard = Arc::new(Settled::default());
    hub.device().on_complete(heard.clone());
    let from_settings = hub
        .device()
        .start_or_join(alice(), crate::subscription::LoginTarget::GlobalUser, None)
        .await
        .unwrap();

    let (events, _) = hub.stream(ALICE, "why is the sky blue?", None, None).await;

    assert_eq!(connect_prompt(&events).flow_id, Some(from_settings.id));
    let flow = hub.device().get(alice(), from_settings.id).unwrap();
    assert!(flow.state.is_pending(), "{:?}", flow.state);
    assert_eq!(
        flow.waiters.len(),
        1,
        "the message waits on the global sign-in"
    );
    assert!(heard.0.lock().is_empty(), "nothing was replaced");
    assert_eq!(codes_issued(&hub).await, 1);
}

/// Every flow the hub settled.
#[derive(Default)]
struct Settled(parking_lot::Mutex<Vec<crate::subscription::DeviceFlow>>);

#[async_trait::async_trait]
impl crate::subscription::DeviceLoginListener for Settled {
    async fn settled(&self, flow: crate::subscription::DeviceFlow) {
        self.0.lock().push(flow);
    }
}

#[tokio::test]
async fn connect_completion_reruns_unanswered_message_once() {
    let hub = hub(|_| {}).await;
    issues_codes(&hub).await;
    hub.stream(ALICE, "first", None, None).await.1.unwrap();
    hub.stream(ALICE, "why is the sky blue?", None, None)
        .await
        .1
        .unwrap();
    let registry = hub.answering();
    grants_sign_in(&hub).await;

    hub.device()
        .tick(registry.as_ref(), chrono::Utc::now())
        .await;

    hub.answered_with(SKY).await;
    assert_eq!(
        hub.connected_reruns(),
        1,
        "the joined messages are answered once"
    );
    assert_eq!(hub.chatgpt_requests().await, 1);
}

#[tokio::test]
async fn connect_completion_skips_answered_waiter() {
    let hub = hub(|_| {}).await;
    issues_codes(&hub).await;
    hub.stream(ALICE, "first", None, None).await.1.unwrap();
    hub.chatgpt_login(ALICE).await;
    assert_eq!(hub.ask(ALICE, "again").await.unwrap(), SKY);
    let registry = hub.answering();
    grants_sign_in(&hub).await;

    hub.device()
        .tick(registry.as_ref(), chrono::Utc::now())
        .await;

    assert_eq!(hub.connected_reruns(), 0);
    assert_eq!(hub.chatgpt_requests().await, 1);
}

#[tokio::test]
async fn completion_reruns_only_messages_on_agents_its_login_serves() {
    let hub = hub(|_| {}).await;
    issues_codes(&hub).await;
    let (events, _) = hub.stream(ALICE, "why is the sky blue?", None, None).await;
    let registry = hub.answering();
    let mut flow = hub
        .device()
        .get(alice(), connect_prompt(&events).flow_id.unwrap())
        .unwrap();
    flow.state = crate::subscription::FlowState::Completed {
        account: Default::default(),
    };

    flow.target = crate::subscription::LoginTarget::AgentUser {
        agent_id: "elsewhere".into(),
    };
    assert!(!connect::reruns_any(&registry, &flow));
    flow.target = crate::subscription::LoginTarget::GlobalUser;
    assert!(connect::reruns_any(&registry, &flow));
}

#[tokio::test]
async fn rerun_skips_a_message_the_person_moved_on_from() {
    let hub = hub(|_| {}).await;
    issues_codes(&hub).await;
    let (events, _) = hub.stream(ALICE, "first", None, None).await;
    let flow = hub
        .device()
        .get(alice(), connect_prompt(&events).flow_id.unwrap())
        .unwrap();
    let waiter: connect::ConnectWaiter = serde_json::from_value(flow.waiters[0].clone()).unwrap();
    hub.chatgpt_login(ALICE).await;
    assert_eq!(hub.ask(ALICE, "never mind").await.unwrap(), SKY);
    let (broadcast, _) = tokio::sync::broadcast::channel(16);

    crate::slack_manager::rerun_unanswered(
        &hub.service,
        &[],
        &crate::session_push::SessionPush::new(None, broadcast),
        &waiter,
        connect::CONNECTED_PROMPT,
    )
    .await
    .unwrap();

    assert_eq!(hub.connected_reruns(), 0);
    assert_eq!(hub.chatgpt_requests().await, 1);
}

/// Every message a surface was asked to send, as `(channel, thread, text)`,
/// and every message it was asked to edit, as `(channel, ts, text)`.
#[derive(Default)]
struct Sent {
    messages: parking_lot::Mutex<Vec<(String, Option<String>, String)>>,
    updates: parking_lot::Mutex<Vec<(String, String, String)>>,
    refuses_dm: bool,
}

impl Sent {
    fn messages(&self) -> Vec<(String, Option<String>, String)> {
        self.messages.lock().clone()
    }

    fn updates(&self) -> Vec<(String, String, String)> {
        self.updates.lock().clone()
    }

    /// Wait for a message carrying `text` to be sent.
    async fn sent_with(&self, text: &str) {
        for _ in 0..200 {
            if self
                .messages()
                .iter()
                .any(|(_, _, sent)| sent.contains(text))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("never sent {text:?}: {:?}", self.messages());
    }
}

#[async_trait::async_trait]
impl workforce_common::channels::ChannelSurface for Sent {
    fn platform(&self) -> &str {
        "slack"
    }
    fn capabilities(&self) -> workforce_common::channels::ChannelCapabilities {
        Default::default()
    }
    async fn add_reaction(&self, _: &workforce_common::channels::Reaction) -> eyre::Result<()> {
        Ok(())
    }
    async fn remove_reaction(&self, _: &workforce_common::channels::Reaction) -> eyre::Result<()> {
        Ok(())
    }
    async fn send_message(
        &self,
        message: &workforce_common::channels::OutboundMessage,
    ) -> eyre::Result<workforce_common::channels::SentMessage> {
        let mut messages = self.messages.lock();
        messages.push((
            message.channel.clone(),
            message.thread_id.clone(),
            format!("{} {:?}", message.text, message.blocks),
        ));
        Ok(workforce_common::channels::SentMessage {
            channel: message.channel.clone(),
            message_id: format!("ts-{}", messages.len()),
            thread_id: message.thread_id.clone(),
            platform: "slack".into(),
        })
    }
    async fn update_message(
        &self,
        channel: &str,
        message_id: &str,
        outbound: &workforce_common::channels::OutboundMessage,
    ) -> eyre::Result<()> {
        self.updates.lock().push((
            channel.to_string(),
            message_id.to_string(),
            outbound.text.clone(),
        ));
        Ok(())
    }
    async fn open_conversation(&self, user_id: &str) -> eyre::Result<String> {
        match self.refuses_dm {
            true => Err(eyre::eyre!("cannot_dm_user")),
            false => Ok(format!("D-{user_id}")),
        }
    }
}

#[tokio::test]
async fn device_code_only_sent_by_dm_in_shared_thread() {
    let hub = hub(|_| {}).await;
    issues_codes(&hub).await;
    let (events, _) = hub.stream(ALICE, "hello", None, None).await;
    let prompt = connect_prompt(&events);
    let session_id = hub.session(ALICE).id;
    let hub = &hub;
    let deliver = |session_context, sent: Arc<Sent>| {
        let mut session = hub.session(ALICE);
        session.context_type = session_context;
        hub.service.sessions.save(&session).unwrap();
        let (session_id, prompt) = (session_id.clone(), prompt.clone());
        async move {
            hub.deliver(
                &sent,
                "T1",
                ("C-shared", Some("1700.1")),
                &session_id,
                &prompt,
            )
            .await;
            sent.messages()
        }
    };

    let sent = deliver(
        Some(workforce_common::session::SessionContextType::Thread),
        Arc::default(),
    )
    .await;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].0, "D-U-ALICE", "the code goes first, by DM");
    assert!(sent[0].2.contains(CODE));
    assert_eq!(sent[1].0, "C-shared", "then the thread hears it was sent");
    assert!(!sent[1].2.contains(CODE), "{:?}", sent[1]);
    assert!(sent[1].2.contains("<@U-ALICE>"));
    let flow = hub.device().get(alice(), prompt.flow_id.unwrap()).unwrap();
    assert_eq!(
        flow.deliveries,
        [crate::subscription::SlackDelivery {
            workspace_id: "T1".into(),
            channel: "D-U-ALICE".into(),
            ts: "ts-1".into()
        }]
    );

    let sent = deliver(
        Some(workforce_common::session::SessionContextType::Thread),
        Arc::new(Sent {
            refuses_dm: true,
            ..Sent::default()
        }),
    )
    .await;
    assert!(
        sent.is_empty(),
        "a thread is not told of a DM that never went: {sent:?}"
    );
}

#[tokio::test]
async fn connect_code_in_dm_posts_inline() {
    let hub = hub(|_| {}).await;
    issues_codes(&hub).await;
    let (events, _) = hub.stream(ALICE, "hello", None, None).await;
    let prompt = connect_prompt(&events);
    let mut session = hub.session(ALICE);
    session.context_type = Some(workforce_common::session::SessionContextType::Dm);
    hub.service.sessions.save(&session).unwrap();
    let sent = Arc::new(Sent::default());

    hub.deliver(&sent, "T1", ("D-ALICE", None), &session.id, &prompt)
        .await;

    let sent = sent.messages();
    assert_eq!(sent.len(), 1, "inline in the DM: {sent:?}");
    assert_eq!(sent[0].0, "D-ALICE");
    assert!(sent[0].2.contains(CODE));
}

#[tokio::test]
async fn a_rerun_the_thread_moved_past_tells_the_waiter_to_ask_again() {
    let hub = hub(|_| {}).await;
    issues_codes(&hub).await;
    let thread_workspace = Arc::new(Sent::default());
    let registry = hub.wired(
        [(
            "T2".to_string(),
            thread_workspace.clone() as Arc<dyn workforce_common::channels::ChannelSurface>,
        )]
        .into(),
    );
    hub.stream_at(THREAD, ALICE, "why is the sky blue?", None, None)
        .await
        .1
        .unwrap();
    let mut session = hub.slack_session(THREAD);
    session.add_user_message("never mind, thanks", None);
    hub.service.sessions.save(&session).unwrap();
    grants_sign_in(&hub).await;

    hub.device()
        .tick(registry.as_ref(), chrono::Utc::now())
        .await;

    assert!(thread_workspace
        .messages()
        .iter()
        .any(|(channel, thread, text)| channel == "C9"
            && thread.as_deref() == Some("1700.1")
            && text.contains("send your message again if you still need it")));
    assert_eq!(hub.chatgpt_requests().await, 0, "nothing is re-run");
}

#[tokio::test]
async fn settle_edits_dm_on_completion_and_expiry() {
    let hub = hub(|_| {}).await;
    issues_codes(&hub).await;
    let (elsewhere, thread_workspace) = (Arc::new(Sent::default()), Arc::new(Sent::default()));
    let surfaces: crate::slack_manager::WorkspaceSurfaces = [
        (
            "T1".to_string(),
            elsewhere.clone() as Arc<dyn workforce_common::channels::ChannelSurface>,
        ),
        ("T2".to_string(), thread_workspace.clone() as _),
    ]
    .into();
    let registry = hub.wired(surfaces);
    let session_id = hub.slack_session(THREAD).id;
    let (hub, thread_ws, session_id) = (&hub, &thread_workspace, session_id.as_str());
    let asked = |message: &'static str| async move {
        let (events, _) = hub.stream_at(THREAD, ALICE, message, None, None).await;
        let prompt = connect_prompt(&events);
        hub.deliver(
            thread_ws,
            "T2",
            (THREAD.1.unwrap(), THREAD.2),
            session_id,
            &prompt,
        )
        .await;
        hub.device().get(alice(), prompt.flow_id.unwrap()).unwrap()
    };

    let expiring = asked("first").await;
    hub.device()
        .tick(registry.as_ref(), expiring.expires_at)
        .await;
    let dm = &expiring.deliveries[0];
    assert_eq!(
        thread_workspace.updates(),
        [(
            dm.channel.clone(),
            dm.ts.clone(),
            "⌛ Code expired; send your message again.".to_string()
        )]
    );
    assert!(thread_workspace
        .messages()
        .iter()
        .any(|(channel, thread, text)| channel == "C9"
            && thread.as_deref() == Some("1700.1")
            && text.contains("expired before it was used")));

    let completing = asked("why is the sky blue?").await;
    grants_sign_in(hub).await;
    hub.device()
        .tick(registry.as_ref(), chrono::Utc::now())
        .await;
    let dm = &completing.deliveries[0];
    assert_eq!(
        thread_workspace.updates().last(),
        Some(&(
            dm.channel.clone(),
            dm.ts.clone(),
            "✅ ChatGPT connected (alice@example.com). Re-running your message.".to_string()
        ))
    );
    thread_workspace.sent_with(SKY).await;
    assert!(
        elsewhere.messages().is_empty() && elsewhere.updates().is_empty(),
        "nothing goes through another workspace"
    );
}

#[tokio::test]
async fn completion_with_nowhere_to_answer_reruns_nothing_and_says_so() {
    let hub = hub(|_| {}).await;
    issues_codes(&hub).await;
    let workspace = Arc::new(Sent::default());
    let registry = hub.wired(
        [(
            "T1".to_string(),
            workspace.clone() as Arc<dyn workforce_common::channels::ChannelSurface>,
        )]
        .into(),
    );
    let (events, _) = hub
        .stream_at(UNTHREADED, ALICE, "why is the sky blue?", None, None)
        .await;
    let prompt = connect_prompt(&events);
    let session = hub.slack_session(UNTHREADED);
    hub.deliver(
        &workspace,
        "T1",
        (UNTHREADED.1.unwrap(), None),
        &session.id,
        &prompt,
    )
    .await;
    grants_sign_in(&hub).await;

    hub.device()
        .tick(registry.as_ref(), chrono::Utc::now())
        .await;

    assert_eq!(
        workspace.updates().last().map(|(_, _, text)| text.as_str()),
        Some("✅ ChatGPT connected (alice@example.com).")
    );
    let session = hub.service.sessions.get(&session.id).unwrap().unwrap();
    assert!(session
        .messages
        .iter()
        .all(|m| m.content != connect::CONNECTED_PROMPT));
    assert_eq!(hub.chatgpt_requests().await, 0);
}

const CODEX_SLOT: &str = "agent_engine:codex";

/// The `config.toml` run `n` of the scripted `codex` was given.
fn codex_config(hub: &Hub, n: usize) -> toml::Table {
    hub.codex.recorded(n, "config").parse().unwrap()
}

/// Alice's ChatGPT login cools, as every route resolving it sees.
fn chatgpt_cooling(hub: &Hub) -> Option<chrono::DateTime<chrono::Utc>> {
    workforce_agent::runner::CURRENT_USER_ID
        .sync_scope(ALICE.to_string(), || {
            hub.service.logins.resolve(SubscriptionKind::ChatGpt)
        })
        .err()
        .and_then(|miss| miss.cooling_until())
}

#[tokio::test]
async fn codex_turn_runs_contained_on_worker_with_shell_and_env_off() {
    let hub = codex_hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;

    let (events, result) = hub.stream(ALICE, "hello", None, None).await;

    let result = result.unwrap();
    assert_eq!(result.response, SCRIPTED_ANSWER);
    assert_eq!(result.model, "codex:gpt-6.1-sol");
    assert_eq!(hub.codex.runs(), 1);
    assert!(hub.claude.calls().is_empty());
    assert_eq!(hub.chatgpt_requests().await, 0);
    let config = codex_config(&hub, 1);
    assert_eq!(config["model"].as_str(), Some("gpt-6.1-sol"));
    assert_eq!(config["sandbox_mode"].as_str(), Some("read-only"));
    assert_eq!(config["web_search"].as_str(), Some("disabled"));
    assert_eq!(
        config["shell_environment_policy"]["inherit"].as_str(),
        Some("none")
    );
    assert_eq!(config["features"]["shell_tool"].as_bool(), Some(false));
    let seats = hub.seats.as_ref().unwrap();
    let ensured = seats.ensured.lock().unwrap().clone();
    assert_eq!(ensured.len(), 1);
    let (project, scope, home) = &ensured[0];
    assert_eq!(project, "_agent-turn");
    assert!(scope.starts_with("at-"), "{scope}");
    assert!(matches!(home, crate::coding_sandbox::HomeSpec::Keyed { key, .. } if key == scope));
    let workdir = PathBuf::from(hub.codex.recorded(1, "cwd").trim());
    assert_eq!(
        std::fs::read_dir(&workdir).unwrap().count(),
        0,
        "an empty workspace: the prompt rides stdin"
    );
    let instructions = hub.codex.recorded(1, "instructions");
    assert!(
        instructions.contains("`mcp__workforce` namespace") && instructions.contains("tool_search"),
        "told how its tools are named to Codex: {instructions}"
    );
    assert!(!instructions.contains("ToolSearch"), "{instructions}");
    assert!(
        !hub.codex.recorded(1, "stdin").contains("## Tool names"),
        "the agent's prompt replaces Codex's own instructions, never the thread's input"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ProgressEvent::Response { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ProgressEvent::Done { .. }))
            .count(),
        1
    );
    let session = hub.session(ALICE);
    let slot = session.engine_slot(CODEX_SLOT).expect("the thread is kept");
    assert_eq!(slot.turns, 1);
    assert_eq!(
        slot.synced_through.as_deref(),
        Some(session.messages.last().unwrap().id.as_str())
    );
}

#[tokio::test]
async fn codex_turn_tools_only_via_gated_mcp() {
    use workforce_agent::tools::approval::testing::{AskEveryCall, RecordingTool, TOOL};
    use workforce_core::policy::PermissionChoice;
    let hub = codex_hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;
    let tool = Arc::new(RecordingTool::default());
    hub.service
        .tools
        .register(Arc::clone(&tool) as Arc<dyn workforce_agent::tools::Tool>);
    hub.service.set_permission_gate(Arc::new(AskEveryCall));
    hub.codex.script(
        None,
        &crate::coding_engine::codex::tests::calling(
            "catalogue_remove_entry",
            json!({ "name": "figma" }),
            &crate::coding_engine::codex::tests::thread(
                "Removed it.",
                &[message_event("Removed it."), completed_event(10, 0, 3)],
            ),
        ),
    );
    let (inject, inbox) = mpsc::channel(8);

    let progress = hub
        .start(
            ("web", None, None),
            ALICE,
            "remove figma",
            None,
            Some(inbox),
            None,
        )
        .await;
    let mut rx = progress.progress_rx;
    let mut prompts = 0;
    while let Some(event) = rx.recv().await {
        if let ProgressEvent::ApprovalRequired { request_id, .. } = event {
            prompts += 1;
            inject
                .send(InjectedMessage::ApprovalResponse {
                    request_id,
                    approved: true,
                    user: ALICE.into(),
                    choice: Some(PermissionChoice::AllowOnce),
                })
                .await
                .unwrap();
        }
    }
    let result = progress.handle.await.unwrap().unwrap();

    assert_eq!(result.response, "Removed it.");
    assert_eq!(
        prompts,
        1,
        "one gate, one prompt: {}",
        hub.codex.file("1.mcp")
    );
    assert_eq!(tool.runs(), [json!({ "name": "figma" })]);
    assert!(
        result
            .tool_invocations
            .iter()
            .any(|invocation| invocation.tool_name == TOOL),
        "the call is the turn's own, under its hub name"
    );
    let config = codex_config(&hub, 1);
    let servers = config["mcp_servers"].as_table().unwrap();
    assert_eq!(
        servers.keys().collect::<Vec<_>>(),
        ["workforce"],
        "the hub's endpoint is its only tool server"
    );
    assert_eq!(
        servers["workforce"]["default_tools_approval_mode"].as_str(),
        Some("approve"),
        "the hub gates each call, so Codex asks no one: its read-only sandbox would refuse them"
    );
    assert_eq!(config["features"]["shell_tool"].as_bool(), Some(false));
}

#[tokio::test]
async fn codex_route_without_worker_hops_unspawned() {
    let hub = hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;
    hub.claude_login(ALICE);

    let (events, result) = hub.stream(ALICE, "hello", None, None).await;

    assert_eq!(result.unwrap().response, "default answer");
    assert_eq!(hub.codex.runs(), 0, "codex never runs off the worker");
    assert_eq!(hub.claude.calls().len(), 1);
    assert!(!events.iter().any(|e| matches!(
        e,
        ProgressEvent::SubActivity { source, .. } if source == "codex"
    )));
}

#[tokio::test]
async fn codex_hop_only_before_output() {
    let hub = codex_hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;
    hub.claude_login(ALICE);
    hub.codex.reply(
        Some(1),
        "",
        &[failed_event("stream disconnected before completion")],
    );
    hub.codex.reply(
        Some(2),
        "",
        &[
            message_event("Looking into it now."),
            failed_event("stream disconnected before completion"),
        ],
    );

    assert_eq!(hub.ask(ALICE, "first").await.unwrap(), "default answer");
    assert_eq!(
        hub.claude.calls().len(),
        1,
        "nothing was produced, so it hops"
    );

    let err = hub.ask(ALICE, "second").await.unwrap_err();
    assert!(err.to_string().contains("stream disconnected"), "{err:#}");
    assert_eq!(hub.codex.runs(), 2);
    assert_eq!(
        hub.claude.calls().len(),
        1,
        "a route that produced output is never handed over"
    );
}

/// Alice's ChatGPT login in `vault`'s per-user slot, when it holds one.
fn stored_login(vault: &crate::vault::Vault) -> Option<crate::subscription::AuthJson> {
    let key = crate::user_config::per_user_key(ALICE, SubscriptionKind::ChatGpt.vault_key());
    vault.sync().unwrap();
    vault
        .get(&key)
        .map(|login| crate::subscription::AuthJson::parse(&login).unwrap())
}

fn stored_refresh(vault: &crate::vault::Vault) -> Option<String> {
    stored_login(vault).map(|login| login.refresh_token().unwrap().to_string())
}

#[tokio::test]
async fn codex_login_written_back_after_round() {
    let hub = codex_hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;
    hub.claude_login(ALICE);
    let vault = hub.service.vault.as_ref().unwrap();
    hub.codex
        .refreshes_to(&crate::subscription::test_support::login(
            &stored_login(vault).unwrap().account_id().unwrap(),
            "a1",
            "rt-1",
        ));
    hub.codex.reply(
        None,
        "",
        &[failed_event("stream disconnected before completion")],
    );

    assert_eq!(hub.ask(ALICE, "hello").await.unwrap(), "default answer");

    assert_eq!(
        crate::subscription::AuthJson::parse(&hub.codex.recorded(1, "auth"))
            .unwrap()
            .refresh_token(),
        Some("rt-0"),
        "the round was handed the stored login"
    );
    assert_eq!(
        stored_refresh(vault).as_deref(),
        Some("rt-1"),
        "the CLI's refresh is kept though its round failed"
    );
}

#[tokio::test]
async fn codex_agent_turn_writes_refresh_back_to_resolved_tier() {
    let hub = codex_hub(codex_first).await;
    let global = hub.service.global_vault.as_ref().unwrap();
    let account = format!("acct-{}", uuid::Uuid::new_v4());
    global
        .set(
            &crate::user_config::per_user_key(ALICE, SubscriptionKind::ChatGpt.vault_key()),
            &crate::subscription::test_support::login(&account, "a0", "rt-0"),
        )
        .unwrap();
    hub.codex
        .refreshes_to(&crate::subscription::test_support::login(
            &account, "a1", "rt-1",
        ));

    assert_eq!(hub.ask(ALICE, "hello").await.unwrap(), SCRIPTED_ANSWER);

    assert_eq!(
        stored_refresh(global).as_deref(),
        Some("rt-1"),
        "kept in the global per-user slot the login resolved from"
    );
    assert_eq!(
        stored_refresh(hub.service.vault.as_ref().unwrap()),
        None,
        "no other tier is written"
    );
}

#[tokio::test]
async fn codex_limit_text_cools_login_and_hops() {
    let hub = codex_hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;
    hub.claude_login(ALICE);
    hub.codex.reply(
        None,
        "",
        &[failed_event(
            "You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), or try again at 3:04 PM.",
        )],
    );

    let (events, result) = hub.stream(ALICE, "hello", None, None).await;

    assert_eq!(result.unwrap().response, "default answer");
    assert_eq!(hub.claude.calls().len(), 1, "the turn hopped to Claude");
    assert!(
        !events.iter().any(|e| matches!(
            e,
            ProgressEvent::StreamDelta { text } if text.contains("usage limit")
        )),
        "the CLI's own notice is never shown"
    );
    assert!(chatgpt_cooling(&hub).is_some(), "the ChatGPT login cools");
}

#[tokio::test]
async fn codex_limit_from_notice_not_model_text() {
    let hub = codex_hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;
    let quoted = "Their error said: You've hit your usage limit. Try again at 3:04 PM.";
    hub.codex.reply(
        Some(1),
        quoted,
        &[message_event(quoted), completed_event(10, 0, 3)],
    );
    hub.codex.reply(
        Some(2),
        "",
        &[
            message_event(quoted),
            failed_event("stream disconnected before completion"),
        ],
    );

    assert_eq!(
        hub.ask(ALICE, "what did the error say?").await.unwrap(),
        quoted
    );
    assert_eq!(chatgpt_cooling(&hub), None, "an answer quoting a limit");

    let err = hub.ask(ALICE, "and again?").await.unwrap_err();
    assert!(err.to_string().contains("stream disconnected"), "{err:#}");
    assert_eq!(
        chatgpt_cooling(&hub),
        None,
        "a failed round is judged by the CLI's notice, never the model's words"
    );
}

#[tokio::test]
async fn codex_resume_across_turns() {
    let hub = codex_hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;
    hub.codex.reply(
        Some(2),
        "Still blue.",
        &[message_event("Still blue."), completed_event(300, 50, 19)],
    );

    let (_, first) = hub.stream(ALICE, "why is the sky blue?", None, None).await;
    let (_, second) = hub.stream(ALICE, "and at noon?", None, None).await;

    let first = first.unwrap();
    assert_eq!(first.total_usage.input_tokens, 100);
    let second = second.unwrap();
    assert_eq!(second.response, "Still blue.");
    assert_eq!(
        (
            second.total_usage.input_tokens,
            second.total_usage.cache_read_input_tokens,
            second.total_usage.output_tokens
        ),
        (150, 30, 12),
        "billed what the turn added to its thread, not the thread's total"
    );
    let first_args = hub.codex.recorded(1, "args");
    assert!(!first_args.lines().any(|a| a == "resume"), "{first_args}");
    let args: Vec<String> = hub
        .codex
        .recorded(2, "args")
        .lines()
        .map(str::to_string)
        .collect();
    assert!(
        args.windows(2).any(|w| w[0] == "resume" && w[1] == "thr_1"),
        "{args:?}"
    );
    let stdin = hub.codex.recorded(2, "stdin");
    assert!(stdin.ends_with("and at noon?"), "{stdin}");
    assert!(
        !stdin.contains("why is the sky blue?"),
        "the thread already holds the first turn: {stdin}"
    );
    assert_eq!(hub.session(ALICE).engine_slot(CODEX_SLOT).unwrap().turns, 2);
}

#[tokio::test]
async fn codex_image_passed_with_i_flag() {
    let hub = codex_hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;
    let png = b"\x89PNG\r\n\x1a\nnot really";
    hub.codex.script(
        None,
        &format!(
            r#"img=$(awk 'prev=="-i"{{print}} {{prev=$0}}' "$dir/$n.args")
cp "$img" "$dir/$n.image"
{answer}"#,
            answer = crate::coding_engine::codex::tests::thread(
                "A red dot.",
                &[message_event("A red dot."), completed_event(10, 0, 3)]
            )
        ),
    );

    let progress = hub
        .start(
            ("web", None, None),
            ALICE,
            "what is this?",
            Some(vec![
                workforce_common::session::SessionAttachment::from_bytes(
                    "dot.png",
                    "image/png",
                    png,
                ),
            ]),
            None,
            None,
        )
        .await;
    let mut rx = progress.progress_rx;
    while rx.recv().await.is_some() {}
    let result = progress.handle.await.unwrap().unwrap();

    assert_eq!(result.response, "A red dot.");
    let args: Vec<String> = hub
        .codex
        .recorded(1, "args")
        .lines()
        .map(str::to_string)
        .collect();
    let prompt = args.iter().position(|a| a == "-").unwrap();
    assert_eq!(
        args[prompt + 1],
        "-i",
        "the image follows the prompt: {args:?}"
    );
    assert!(args[prompt + 2].ends_with(".png"), "{args:?}");
    assert_eq!(args.len(), prompt + 3);
    assert_eq!(
        std::fs::read(hub.codex.program().with_file_name("1.image")).unwrap(),
        png,
        "the image reached the CLI byte for byte"
    );
    let workdir = PathBuf::from(hub.codex.recorded(1, "cwd").trim());
    assert_eq!(
        std::fs::read_dir(&workdir).unwrap().count(),
        0,
        "the image goes with its round"
    );
}

/// Gather the progress of the turn under way in `progress`, stopping it once
/// it has streamed `streamed`, and its result.
async fn stop_once_streamed(
    progress: crate::serve::MessageProgress,
    inject: &mpsc::Sender<InjectedMessage>,
    streamed: &str,
) -> eyre::Result<RunResult> {
    let mut rx = progress.progress_rx;
    let mut stopped = false;
    while let Some(event) = rx.recv().await {
        if !stopped
            && matches!(&event, ProgressEvent::StreamDelta { text } if text.contains(streamed))
        {
            stopped = true;
            inject
                .send(InjectedMessage::SteerStop {
                    text: "stop".into(),
                    user: ALICE.into(),
                })
                .await
                .unwrap();
        }
    }
    progress.handle.await.unwrap()
}

fn sleeping_after(events: &[serde_json::Value]) -> String {
    format!(
        "{}sleep 60\n",
        crate::coding_engine::codex::tests::thread("", events)
    )
}

#[tokio::test]
async fn codex_stop_resumes_the_cut_thread() {
    let hub = codex_hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;
    hub.codex
        .script(Some(1), &sleeping_after(&[message_event("Working on it.")]));
    hub.codex.reply(
        Some(2),
        "Stopped as asked.",
        &[
            message_event("Stopped as asked."),
            completed_event(10, 0, 3),
        ],
    );
    let (inject, inbox) = mpsc::channel(8);

    let progress = hub
        .start(
            ("web", None, None),
            ALICE,
            "do the long thing",
            None,
            Some(inbox),
            None,
        )
        .await;
    let result = stop_once_streamed(progress, &inject, "Working on it.").await;

    assert_eq!(result.unwrap().response, "Stopped as asked.");
    assert_eq!(hub.codex.runs(), 2);
    let args: Vec<String> = hub
        .codex
        .recorded(2, "args")
        .lines()
        .map(str::to_string)
        .collect();
    assert!(
        args.windows(2).any(|w| w == ["resume", "thr_1"]),
        "the cut thread is resumed: {args:?}"
    );
    let stdin = hub.codex.recorded(2, "stdin");
    assert!(stdin.contains("The user asked you to stop"), "{stdin}");
    assert!(
        !stdin.contains("do the long thing"),
        "the thread already holds the task, and its calls: {stdin}"
    );
}

#[tokio::test]
async fn codex_stop_before_the_thread_starts_replays_the_task() {
    let hub = codex_hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;
    hub.codex.script(Some(1), "sleep 60\n");
    hub.codex.reply(
        Some(2),
        "Stopped as asked.",
        &[
            message_event("Stopped as asked."),
            completed_event(10, 0, 3),
        ],
    );
    let (tx, rx) = mpsc::channel(8);
    let running = hub.codex.started(1);
    let stop = tokio::spawn(async move {
        running.await;
        tx.send(InjectedMessage::SteerStop {
            text: "stop".into(),
            user: ALICE.into(),
        })
        .await
        .unwrap();
        tx
    });

    let (_, result) = hub.stream(ALICE, "do the long thing", Some(rx), None).await;
    drop(stop.await);

    assert_eq!(result.unwrap().response, "Stopped as asked.");
    assert_eq!(
        hub.codex.runs(),
        2,
        "the round asking for a thread that never started runs nothing"
    );
    let args = hub.codex.recorded(2, "args");
    assert!(!args.lines().any(|a| a == "resume"), "{args}");
    let stdin = hub.codex.recorded(2, "stdin");
    for part in ["do the long thing", "The user asked you to stop"] {
        assert!(stdin.contains(part), "{part:?} in {stdin}");
    }
}

#[tokio::test]
async fn codex_stop_after_the_turn_completed_bills_it_once() {
    let hub = codex_hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;
    hub.codex.script(
        Some(1),
        &sleeping_after(&[
            completed_event(120, 20, 7),
            message_event("Done, wrapping up."),
        ]),
    );
    hub.codex.reply(
        Some(2),
        "Stopped as asked.",
        &[
            message_event("Stopped as asked."),
            completed_event(200, 30, 10),
        ],
    );
    let (inject, inbox) = mpsc::channel(8);

    let progress = hub
        .start(
            ("web", None, None),
            ALICE,
            "do the long thing",
            None,
            Some(inbox),
            None,
        )
        .await;
    let result = stop_once_streamed(progress, &inject, "Done, wrapping up.")
        .await
        .unwrap();

    assert_eq!(result.response, "Stopped as asked.");
    assert_eq!(
        (
            result.total_usage.input_tokens,
            result.total_usage.cache_read_input_tokens,
            result.total_usage.output_tokens
        ),
        (170, 30, 10),
        "the thread's whole use, its cut turn billed once"
    );
}

#[tokio::test]
async fn codex_resumed_turn_never_repeats_the_prompt_in_the_thread() {
    let hub = codex_hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;

    assert_eq!(
        hub.ask(ALICE, "why is the sky blue?").await.unwrap(),
        SCRIPTED_ANSWER
    );
    assert_eq!(
        hub.ask(ALICE, "and at noon?").await.unwrap(),
        SCRIPTED_ANSWER
    );

    let args = hub.codex.recorded(2, "args");
    assert!(args.lines().any(|a| a == "resume"), "{args}");
    let stdin = hub.codex.recorded(2, "stdin");
    assert!(
        !stdin.contains("## Tool names") && !stdin.contains("# Instructions"),
        "the prompt never enters the thread: {stdin}"
    );
    let config = codex_config(&hub, 2);
    let file = config["model_instructions_file"].as_str().unwrap();
    assert!(file.ends_with("instructions.md"), "{file}");
    assert!(
        hub.codex
            .recorded(2, "instructions")
            .contains("## Tool names"),
        "a resumed round reads the prompt afresh"
    );
}

#[tokio::test]
async fn codex_turn_names_an_image_it_cannot_show() {
    let hub = codex_hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;
    let svg = b"<svg xmlns='http://www.w3.org/2000/svg'/>";

    let progress = hub
        .start(
            ("web", None, None),
            ALICE,
            "what is this?",
            Some(vec![
                workforce_common::session::SessionAttachment::from_bytes(
                    "logo.svg",
                    "image/svg+xml",
                    svg,
                ),
            ]),
            None,
            None,
        )
        .await;
    let mut rx = progress.progress_rx;
    while rx.recv().await.is_some() {}
    progress.handle.await.unwrap().unwrap();

    let args = hub.codex.recorded(1, "args");
    assert!(
        !args.lines().any(|a| a == "-i"),
        "only raster images are shown: {args}"
    );
    let stdin = hub.codex.recorded(1, "stdin");
    assert!(
        stdin.contains("image/svg+xml image can't be shown here"),
        "{stdin}"
    );
}

#[tokio::test]
async fn advisor_model_backend_on_codex_primary_runs_on_the_cli() {
    let hub = codex_hub(codex_first).await;
    hub.chatgpt_login(ALICE).await;
    hub.codex.reply(
        None,
        "The plan holds.",
        &[message_event("The plan holds."), completed_event(10, 0, 3)],
    );
    let config = workforce_common::settings::AdvisorConfig {
        enabled: true,
        source: workforce_common::settings::AdvisorSource::Model,
        ..Default::default()
    };
    let request = crate::advisor::AdvisorRequest {
        payload: workforce_common::settings::AdvisorPayload::Transcript,
        payload_text: "user: ship it?".into(),
        payload_truncated: false,
        question: Some("Is the plan sound?".into()),
        lenses: Vec::new(),
    };

    let outcome = workforce_agent::runner::CURRENT_USER_ID
        .scope(
            ALICE.to_string(),
            crate::advisor::dispatch(
                &hub.service,
                &config,
                &request,
                Some(ALICE),
                &std::sync::Arc::default(),
                None,
            ),
        )
        .await;

    match outcome {
        crate::advisor::AdvisorOutcome::Advice(advice) => {
            assert_eq!(advice.text, "The plan holds.")
        }
        crate::advisor::AdvisorOutcome::Unavailable(failure) => {
            panic!("the review ran on the agent's Codex route: {failure:?}")
        }
    }
    assert_eq!(hub.codex.runs(), 1);
    assert!(
        hub.codex
            .recorded(1, "args")
            .lines()
            .any(|a| a == "--ephemeral"),
        "a tool loop keeps no thread"
    );
    assert_eq!(hub.chatgpt_requests().await, 0);
}

#[tokio::test]
async fn interactive_codex_turn_without_login_starts_chatgpt_flow() {
    let hub = codex_hub(|config| {
        codex_first(config);
        config.llm.failover.clear();
    })
    .await;
    issues_codes(&hub).await;

    let (events, result) = hub.stream(ALICE, "why is the sky blue?", None, None).await;

    let notice = result.unwrap().response;
    assert!(notice.contains("ChatGPT"), "{notice}");
    let prompt = connect_prompt(&events);
    assert_eq!(prompt.kind, SubscriptionKind::ChatGpt);
    assert!(prompt.flow_id.is_some(), "a sign-in was started");
    assert!(!notice.contains(CODE));
    assert_eq!(hub.codex.runs(), 0, "nothing runs without a login");
}

/// A 4s clip with no audio, or `None` without ffmpeg.
fn tiny_clip() -> Option<Vec<u8>> {
    if !workforce_agent::ffmpeg::ffmpeg_available() {
        eprintln!("skipping: ffmpeg not installed");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("pan.mp4");
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
        .args([
            "-i",
            "testsrc=duration=4:size=96x64:rate=10",
            "-c:v",
            "libx264",
        ])
        .args(["-pix_fmt", "yuv420p"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    Some(std::fs::read(out).unwrap())
}

fn clip_attachment(bytes: &[u8]) -> Option<Vec<workforce_common::session::SessionAttachment>> {
    Some(vec![
        workforce_common::session::SessionAttachment::from_bytes("pan.mp4", "video/mp4", bytes),
    ])
}

/// The image blocks and frame labels the `claude` call carried.
fn sent_frames(claude: &ScriptedClaude) -> (usize, usize) {
    let stdin = &claude.calls()[0].stdin;
    let content = stdin["message"]["content"].as_array().unwrap();
    let images = content.iter().filter(|b| b["type"] == "image").count();
    let labels = content
        .iter()
        .filter_map(|b| b["text"].as_str())
        .filter(|t| t.contains("of `pan.mp4` at"))
        .count();
    (images, labels)
}

#[tokio::test]
async fn a_video_reaches_the_model_as_frames_on_the_sync_turn() {
    let Some(clip) = tiny_clip() else { return };
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.claude.reply(None, &answer("A test pattern."));

    hub.service
        .process_message(
            "web",
            ALICE,
            "what does it show?",
            None,
            None,
            None,
            None,
            clip_attachment(&clip),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(sent_frames(&hub.claude), (2, 2), "4s at 2s spacing");
}

#[tokio::test]
async fn a_video_reaches_the_model_as_frames_on_the_streamed_turn() {
    let Some(clip) = tiny_clip() else { return };
    let hub = hub(claude_first).await;
    hub.claude_login(ALICE);
    hub.claude.reply(None, &answer("A test pattern."));

    let progress = hub
        .start(
            ("web", None, None),
            ALICE,
            "what does it show?",
            clip_attachment(&clip),
            None,
            None,
        )
        .await;
    let mut rx = progress.progress_rx;
    while rx.recv().await.is_some() {}
    progress.handle.await.unwrap().unwrap();

    assert_eq!(sent_frames(&hub.claude), (2, 2));
}

/// The image parts and frame labels the first ChatGPT request carried.
async fn sent_chatgpt_frames(hub: &Hub) -> (usize, usize) {
    let requests = hub.server.received_requests().await.unwrap();
    let body: serde_json::Value = requests
        .iter()
        .find(|r| r.url.path() == "/responses")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .expect("the ChatGPT backend was called");
    let parts: Vec<&serde_json::Value> = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .collect();
    let images = parts.iter().filter(|p| p["type"] == "input_image").count();
    let labels = parts
        .iter()
        .filter_map(|p| p["text"].as_str())
        .filter(|t| t.contains("of `pan.mp4` at"))
        .count();
    (images, labels)
}

/// A hub whose agent's model is `model` on the `chatgpt` route, with the
/// describer on the same route, as an install on ChatGPT plans alone has it.
async fn chatgpt_hub(model: &'static str) -> Hub {
    let hub = hub(move |config| {
        config.llm.default_model = model.into();
        config.llm.vision = Some(toml::from_str(r#"model = "chatgpt:gpt-6.1-sol""#).unwrap());
    })
    .await;
    hub.chatgpt_login(ALICE).await;
    hub
}

#[tokio::test]
async fn a_video_reaches_a_chatgpt_model_as_frames() {
    let Some(clip) = tiny_clip() else { return };
    for model in ["gpt-6.1-sol", "chatgpt:gpt-6.1-sol"] {
        let hub = chatgpt_hub(model).await;

        hub.service
            .process_message(
                "web",
                ALICE,
                "what does it show?",
                None,
                None,
                None,
                None,
                clip_attachment(&clip),
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            sent_chatgpt_frames(&hub).await,
            (2, 2),
            "{model} sees the frames itself"
        );
        assert_eq!(
            hub.chatgpt_requests().await,
            1,
            "{model}: no frame is sent to be described"
        );
    }
}

#[tokio::test]
async fn a_video_reaches_a_chatgpt_model_as_frames_on_the_streamed_turn() {
    let Some(clip) = tiny_clip() else { return };
    let hub = chatgpt_hub("gpt-6.1-sol").await;

    let progress = hub
        .start(
            ("web", None, None),
            ALICE,
            "what does it show?",
            clip_attachment(&clip),
            None,
            None,
        )
        .await;
    let mut rx = progress.progress_rx;
    while rx.recv().await.is_some() {}
    progress.handle.await.unwrap().unwrap();

    assert_eq!(sent_chatgpt_frames(&hub).await, (2, 2));
}

/// Every request the ChatGPT backend received, in order.
async fn chatgpt_bodies(hub: &Hub) -> Vec<serde_json::Value> {
    hub.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/responses")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// A turn from `ALICE` in the web chat carrying one attachment.
async fn ask_with(hub: &Hub, filename: &str, mime: &str) -> eyre::Result<String> {
    hub.service
        .process_message(
            "web",
            ALICE,
            "what is this?",
            None,
            None,
            None,
            None,
            Some(vec![
                workforce_common::session::SessionAttachment::from_bytes(filename, mime, b"bytes"),
            ]),
            None,
            None,
            None,
            None,
            None,
        )
        .await
}

#[tokio::test]
async fn an_image_for_a_vision_model_on_a_text_only_route_is_described_not_dropped() {
    let hub = hub(|config| {
        config.llm.provider = "ollama".into();
        config.llm.default_model = "gpt-6.1-sol".into();
        config.llm.failover = vec!["chatgpt:gpt-6.1-sol".into()];
        config.llm.vision = Some(toml::from_str(r#"model = "chatgpt:gpt-6.1-sol""#).unwrap());
    })
    .await;
    hub.chatgpt_login(ALICE).await;

    ask_with(&hub, "shot.png", "image/png").await.unwrap();

    let bodies = chatgpt_bodies(&hub).await;
    let images = |body: &serde_json::Value| body.to_string().matches("input_image").count();
    let (described, turns) = bodies.split_first().expect("the describer was called");
    assert_eq!(
        images(described),
        1,
        "the describer is shown the image, on the login of the person the turn is for"
    );
    assert!(!turns.is_empty(), "the turn was answered");
    for turn in turns {
        assert_eq!(images(turn), 0, "the turn carries the description");
        assert!(
            turn.to_string()
                .contains("Image `shot.png` (described by gpt-6.1-sol)"),
            "{turn}"
        );
    }
}

#[tokio::test]
async fn a_misconfigured_transcriber_tells_the_model_and_the_turn_still_runs() {
    let hub = hub(|config| {
        config.llm.transcription = Some(toml::from_str(r#"model = "whisper-1""#).unwrap());
    })
    .await;
    hub.chatgpt_login(ALICE).await;

    ask_with(&hub, "memo.m4a", "audio/mp4").await.unwrap();

    let sent = chatgpt_bodies(&hub).await[0].to_string();
    assert!(
        sent.contains(
            "`memo.m4a` was not transcribed: transcription is misconfigured: \
             [llm.transcription] model 'whisper-1' names no known provider"
        ),
        "{sent}"
    );
}

#[tokio::test]
async fn the_install_switch_keeps_frames_off_a_streamed_turn() {
    let Some(clip) = tiny_clip() else { return };
    let hub = hub(|config| {
        claude_first(config);
        config.attachments.video.enabled = false;
    })
    .await;
    hub.claude_login(ALICE);
    hub.claude.reply(None, &answer("I cannot see it."));

    let progress = hub
        .start(
            ("web", None, None),
            ALICE,
            "what does it show?",
            clip_attachment(&clip),
            None,
            None,
        )
        .await;
    let mut rx = progress.progress_rx;
    while rx.recv().await.is_some() {}
    progress.handle.await.unwrap().unwrap();

    assert_eq!(sent_frames(&hub.claude), (0, 0));
}
