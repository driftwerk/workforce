//! Claude Code on the user's subscription token: the engine every coding
//! surface ran on before there was a choice.

#[cfg(test)]
use crate::serve::Compose as _;
use async_trait::async_trait;
use eyre::{eyre, Result};
use tokio_util::sync::CancellationToken;
use workforce_agent::runner::ProgressEvent;

use std::sync::Arc;

use workforce_common::settings::CLAUDE_CODE_EFFORTS;
use workforce_common::subscription::{SubscriptionKind, SubscriptionRoute};

use super::{
    labelled_model, CliRoundResult, CodingEngine, CodingRound, Continuity, ModelChoices, Refusal,
    RoundContext,
};
use crate::claude_code::{self, ClaudeAuth, ClaudeCodeConfig, CliHealth};
use crate::coding_sandbox::CodingSandbox;
use crate::mcp_turn::{ClientTimeout, RoundTools};
use crate::serve::AgentService;
use crate::subscription::limits;

pub const LABEL: &str = "claude-code";

#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeCodeEngine;

/// The CLI invocation for `round` in `sandbox`, served `mcp` and paid by
/// `auth`: the sandbox says where it runs, the round everything else.
pub(crate) fn config_for(
    sandbox: &dyn CodingSandbox,
    round: &CodingRound,
    mcp: Option<RoundTools>,
    auth: ClaudeAuth,
) -> ClaudeCodeConfig {
    let mcp_tools = mcp.iter().flat_map(RoundTools::allowed_tools);
    let (session_id, resume) = match round.continuity {
        Continuity::OneShot => (None, false),
        Continuity::Session { id, resume } => (Some(id), resume),
    };
    ClaudeCodeConfig {
        task: round.task.clone(),
        workdir: sandbox.workdir().to_path_buf(),
        max_turns: round.max_turns,
        allowed_tools: round.tools.iter().cloned().chain(mcp_tools).collect(),
        append_system_prompt: round.system_prompt.clone(),
        timeout: round.timeout,
        model: round.model.clone(),
        effort: round.effort.clone(),
        mcp,
        session_id,
        resume,
        persist_session: session_id.is_some(),
        load_claude_md: round.memory.claude_md,
        load_auto_memory: round.memory.auto_memory,
        gh_token: round.gh_token.clone(),
        cache_dir: round.cache_dir.clone(),
        remote: sandbox.placement(),
        auth,
        jobs: round.jobs.clone(),
        contained: None,
    }
}

/// The model id a round asking for `requested` runs on, as the vision
/// registry knows it: the CLI's aliases resolve through the agent's
/// Anthropic tier table, which is what they stand for on a subscription, and
/// anything else is taken as the id it names.
pub(crate) fn vision_model(requested: Option<&str>, service: &AgentService) -> String {
    let requested = requested.unwrap_or("sonnet");
    super::bedrock::alias_tier(requested)
        .and_then(|tier| {
            service
                .tier_registry()
                .resolve_for_tier(tier, ANTHROPIC_TIERS)
        })
        .unwrap_or_else(|| requested.to_string())
}

/// The tier table the CLI's aliases resolve in on a subscription.
const ANTHROPIC_TIERS: &str = "anthropic";

/// The models and efforts the Claude Code CLI accepts by name.
pub(super) fn claude_model_choices() -> ModelChoices {
    ModelChoices {
        models: ["opus", "sonnet", "haiku", "fable"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        efforts: CLAUDE_CODE_EFFORTS,
        raw_id_prefixes: &["claude-"],
    }
}

/// Whether Claude Code's failure text says the subscription's window or the
/// API's rate limit is spent.
pub(super) fn claude_limit_reached(text: &str) -> bool {
    limits::is_rate_limited(text) || limits::is_usage_capped(text)
}

#[async_trait]
impl CodingEngine for ClaudeCodeEngine {
    fn label(&self) -> &'static str {
        LABEL
    }

    fn preflight(&self, ctx: &RoundContext<'_>) -> Result<(), Refusal> {
        ctx.login(SubscriptionKind::Claude).map(drop)
    }

    fn limit_reached(&self, text: &str) -> bool {
        claude_limit_reached(text)
    }

    fn model_choices(&self) -> ModelChoices {
        claude_model_choices()
    }

    async fn cli_health(&self) -> Option<CliHealth> {
        Some(claude_code::probe_cli().await)
    }

    async fn run(
        &self,
        sandbox: Arc<dyn CodingSandbox>,
        round: &CodingRound,
        ctx: &RoundContext<'_>,
        cancel: Option<CancellationToken>,
        progress: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) -> Result<CliRoundResult> {
        let started_at = chrono::Utc::now();
        let collected = round
            .collecting()
            .then(|| {
                ctx.service
                    .logins
                    .holding(SubscriptionKind::Claude, &|_| true)
            })
            .flatten();
        let login = match collected {
            Some(login) => login,
            None => ctx
                .login(SubscriptionKind::Claude)
                .map_err(|refusal| eyre!(refusal.reason))?,
        };
        let mcp = ctx
            .round_tools(
                sandbox.as_ref(),
                cancel.as_ref(),
                ClientTimeout::CLAUDE_CODE,
            )
            .map(|tools| tools.with_model(vision_model(round.model.as_deref(), ctx.service)));
        let config = config_for(
            sandbox.as_ref(),
            round,
            mcp,
            ClaudeAuth::Resolved(login.clone()),
        );
        let result = claude_code::run_coding_round(
            &config,
            ctx.service.vault.as_deref(),
            ctx.service.global_vault.as_deref(),
            cancel,
            progress,
        )
        .await;
        result
            .inspect_err(|e| ctx.record_error(self, round, started_at, e))
            .map(|result| {
                let ledger_model = labelled_model(LABEL, result.model.as_deref());
                // Priced from the round's own tokens: the CLI's `total_cost_usd`
                // is the whole resumed transcript's, not this round's.
                let result = CliRoundResult {
                    engine: LABEL.to_string(),
                    cost_usd: ctx.service.cost_tracker.calculate_cost(
                        &ledger_model,
                        result.input_tokens,
                        result.output_tokens,
                        result.cache_creation_tokens,
                        result.cache_read_tokens,
                    ),
                    ..result
                };
                login.settle(&result.round_end(), ctx.service.agent_id.as_str());
                ctx.record(
                    LABEL,
                    &ledger_model,
                    &result,
                    started_at,
                    Some(
                        login.served(SubscriptionRoute::ClaudeCode, ctx.service.agent_id.as_str()),
                    ),
                );
                result
            })
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use uuid::Uuid;

    use crate::claude_code::tests::scripted::{assistant, print, ScriptedClaude};
    use crate::coding_engine::MemoryLoad;
    use crate::coding_sandbox::{DirEntry, ExecOutcome, SandboxCommand};
    use crate::projects::setup::SetupReport;

    pub(in crate::coding_engine) struct StubSandbox;

    #[async_trait]
    impl CodingSandbox for StubSandbox {
        fn workdir(&self) -> &Path {
            Path::new("/work/rails")
        }
        fn setup(&self) -> &[SetupReport] {
            &[]
        }
        fn hub_tool_patterns(&self) -> Vec<String> {
            vec!["jira.*".into()]
        }
        fn fresh(&self) -> bool {
            true
        }
        async fn exec(&self, _: SandboxCommand) -> Result<ExecOutcome> {
            unreachable!()
        }
        async fn read(&self, _: &Path, _: usize) -> Result<Option<String>> {
            unreachable!()
        }
        async fn write(&self, _: &Path, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn list(&self, _: &Path) -> Result<Vec<DirEntry>> {
            unreachable!()
        }
        fn placement(&self) -> Option<crate::claude_code::RemotePlacement> {
            None
        }
    }

    #[test]
    fn the_config_takes_placement_from_the_sandbox_and_the_rest_from_the_round() {
        let id = Uuid::new_v4();
        let round = CodingRound {
            model: Some("fable".into()),
            effort: Some("high".into()),
            tools: vec!["Read".into(), "Bash".into()],
            system_prompt: Some("be brief".into()),
            continuity: Continuity::Session { id, resume: true },
            memory: MemoryLoad {
                claude_md: true,
                auto_memory: false,
            },
            gh_token: Some("ghp".into()),
            max_turns: Some(7),
            cache_dir: Some(PathBuf::from("/cache")),
            ..CodingRound::one_shot("fix it".into(), Duration::from_secs(90))
        };
        let login = crate::subscription::test_support::claude_login("sk-ant-oat01-config");
        let config = config_for(
            &StubSandbox,
            &round,
            None,
            ClaudeAuth::Resolved(login.clone()),
        );
        assert!(matches!(&config.auth, ClaudeAuth::Resolved(paid) if paid.key == login.key));
        assert_eq!(config.workdir, Path::new("/work/rails"));
        assert!(config.mcp.is_none());
        assert_eq!(config.allowed_tools, vec!["Read", "Bash"]);
        assert_eq!(config.session_id, Some(id));
        assert!(config.resume && config.persist_session && config.load_claude_md);
        assert!(!config.load_auto_memory);
        assert_eq!(config.max_turns, Some(7));
        assert_eq!(config.timeout, Duration::from_secs(90));
        assert_eq!(config.model.as_deref(), Some("fable"));
        assert_eq!(config.cache_dir.as_deref(), Some(Path::new("/cache")));
        assert!(config.remote.is_none());

        let one_shot = config_for(
            &StubSandbox,
            &CodingRound::one_shot("t".into(), Duration::from_secs(1)),
            None,
            ClaudeAuth::Resolved(login),
        );
        assert_eq!(one_shot.session_id, None);
        assert!(!one_shot.persist_session && !one_shot.resume);
    }

    const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";

    /// A service with an agent and a global vault, its config adjusted by
    /// `configure`, in `dir`.
    async fn service_in(
        dir: &Path,
        configure: impl FnOnce(&mut workforce_common::settings::WorkforceConfig),
    ) -> std::sync::Arc<crate::serve::AgentService> {
        let server = wiremock::MockServer::start().await;
        crate::serve::testing::chatgpt_service(dir, &server, configure).await
    }

    fn context(service: &crate::serve::AgentService) -> RoundContext<'_> {
        RoundContext {
            service,
            identity: None,
            stand_in: workforce_common::subscription::SCHEDULER,
            ticket_id: None,
            description: "coding.session",
            resume: crate::mcp_gate::RouteResume::Nowhere,
        }
    }

    /// One round of `service`'s Claude Code engine on `claude`, as
    /// `principal`.
    async fn round_as(
        service: &crate::serve::AgentService,
        claude: &ScriptedClaude,
        principal: &workforce_common::session::TurnPrincipal,
    ) -> Result<CliRoundResult> {
        let round = CodingRound {
            model: Some("sonnet".into()),
            ..CodingRound::one_shot("fix it".into(), Duration::from_secs(30))
        };
        run_as(service, claude, principal, &round, None).await
    }

    /// `round` on `service`'s Claude Code engine on `claude`, as `principal`,
    /// reporting to `progress`.
    async fn run_as(
        service: &crate::serve::AgentService,
        claude: &ScriptedClaude,
        principal: &workforce_common::session::TurnPrincipal,
        round: &CodingRound,
        progress: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) -> Result<CliRoundResult> {
        let workdir = tempfile::tempdir().unwrap();
        let sandbox =
            crate::coding_sandbox::adopt_workdir(workdir.path().to_path_buf(), Vec::new(), None);
        crate::user_config::as_principal(
            principal,
            claude.scoped(ClaudeCodeEngine.run(sandbox, round, &context(service), None, progress)),
        )
        .await
    }

    /// The agent's shared Claude login on `service`, as stored.
    fn shared_login(service: &crate::serve::AgentService) -> String {
        let token = unique("shared");
        service
            .vault
            .as_ref()
            .unwrap()
            .set(SubscriptionKind::Claude.vault_key(), &token)
            .unwrap();
        token
    }

    /// Whether Alice's Claude calls on `service` still have a login to run on.
    async fn serves_alice(
        service: &crate::serve::AgentService,
    ) -> Result<(), crate::subscription::NoLogin> {
        crate::user_config::as_principal(&alice(), async {
            service.logins.resolve(SubscriptionKind::Claude).map(drop)
        })
        .await
    }

    #[tokio::test]
    async fn collected_claude_round_survives_a_cooling_login() {
        let tmp = tempfile::tempdir().unwrap();
        let service = service_in(tmp.path(), |_| {}).await;
        let token = unique("alice");
        service
            .global_vault
            .as_ref()
            .unwrap()
            .set(
                &crate::user_config::per_user_key(ALICE, SubscriptionKind::Claude.vault_key()),
                &token,
            )
            .unwrap();
        crate::subscription::CooldownStore::global().note(
            &crate::subscription::LoginKey::of(&crate::subscription::Credential::Token(
                token.clone(),
            ))
            .unwrap(),
            &crate::subscription::LimitSignal {
                status: crate::subscription::LimitStatus::Rejected,
                resets_at: Some(chrono::Utc::now() + chrono::TimeDelta::hours(1)),
                window: None,
            },
            chrono::Utc::now(),
        );
        let claude = ScriptedClaude::new();
        let started = chrono::Utc::now();
        let collected = CodingRound {
            jobs: crate::claude_code::RoundJobs::resuming("r1", started, "r1.0.0"),
            ..CodingRound::one_shot("fix it".into(), Duration::from_secs(30))
        };

        run_as(&service, &claude, &alice(), &collected, None)
            .await
            .expect("the job already running goes on");

        assert_eq!(claude.calls()[0].token, token);
        let rows = super::super::tests::cost_rows(&service);
        assert_eq!(
            rows[0]
                .credential_tier
                .map(workforce_common::subscription::CredentialTier::as_str),
            Some("global_user")
        );
    }

    #[tokio::test]
    async fn timed_out_round_with_401_in_stderr_does_not_kill_login() {
        let tmp = tempfile::tempdir().unwrap();
        let service = service_in(tmp.path(), |_| {}).await;
        shared_login(&service);
        let claude = ScriptedClaude::new();
        claude.script(
            None,
            &format!(
                "printf '%s\\n' '2026-10-02T10:14:12.401Z WARN mcp: 401 Unauthorized from a tool server' >&2\n{}sleep 600\n",
                print(&[assistant("msg-1", "working on it")])
            ),
        );
        let budget = Duration::from_secs(300);
        let round = CodingRound {
            model: Some("sonnet".into()),
            ..CodingRound::one_shot("fix it".into(), budget)
        };
        let (progress, mut reported) = tokio::sync::mpsc::channel(8);
        let alice = alice();

        // The budget runs out on the test's clock, once the round has shown
        // its work, rather than on a wall clock racing the CLI's startup.
        let (timed_out, ()) = tokio::join!(
            run_as(&service, &claude, &alice, &round, Some(progress)),
            async {
                reported.recv().await.expect("the round reports its work");
                tokio::time::pause();
                tokio::time::advance(budget).await;
                tokio::time::resume();
            }
        );
        let timed_out = timed_out.unwrap();

        assert_eq!(
            timed_out.completion,
            crate::claude_code::Completion::Timeout
        );
        assert!(
            timed_out.stderr_tail.contains("401 Unauthorized"),
            "{}",
            timed_out.stderr_tail
        );
        serves_alice(&service)
            .await
            .expect("a round that ran out of time says nothing of its login");
    }

    #[tokio::test]
    async fn auth_error_result_marks_login_dead() {
        let tmp = tempfile::tempdir().unwrap();
        let service = service_in(tmp.path(), |_| {}).await;
        shared_login(&service);
        let claude = ScriptedClaude::new();
        claude.reply(
            None,
            &[serde_json::json!({
                "type": "result",
                "subtype": "success",
                "is_error": true,
                "num_turns": 1,
                "result": "API Error: 401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"OAuth token has expired.\"}}",
            })],
        );

        round_as(&service, &claude, &alice()).await.unwrap();

        let no_login = serves_alice(&service)
            .await
            .expect_err("the refused login is skipped");
        assert!(
            no_login
                .misses
                .iter()
                .any(|(_, miss)| *miss == crate::subscription::Miss::Invalid),
            "{no_login}"
        );
    }

    fn alice() -> workforce_common::session::TurnPrincipal {
        workforce_common::session::TurnPrincipal::run_as(Some(ALICE.to_string()))
    }

    fn unique(name: &str) -> String {
        format!("sk-ant-oat01-{name}-{}", Uuid::new_v4().simple())
    }

    #[tokio::test]
    async fn cc_round_resolves_by_placement_and_policy() {
        use workforce_common::subscription::CredentialTier;
        let claude = ScriptedClaude::new();
        for (tiers, paid_by) in [
            (None, CredentialTier::AgentUser),
            (
                Some(vec![CredentialTier::GlobalUser]),
                CredentialTier::GlobalUser,
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let service = service_in(tmp.path(), |cfg| {
                if let Some(tiers) = tiers {
                    cfg.llm.logins.tiers = tiers;
                }
            })
            .await;
            let key = crate::user_config::per_user_key(ALICE, SubscriptionKind::Claude.vault_key());
            let (on_agent, everywhere) = (unique("agent"), unique("global"));
            service
                .vault
                .as_ref()
                .unwrap()
                .set(&key, &on_agent)
                .unwrap();
            service
                .global_vault
                .as_ref()
                .expect("a global vault")
                .set(&key, &everywhere)
                .unwrap();

            round_as(&service, &claude, &alice()).await.unwrap();

            let expected = match paid_by {
                CredentialTier::AgentUser => &on_agent,
                _ => &everywhere,
            };
            assert_eq!(
                &claude.calls().last().unwrap().token,
                expected,
                "{paid_by:?}"
            );
            let rows = super::super::tests::cost_rows(&service);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].payer.as_deref(), Some(ALICE));
            assert_eq!(
                rows[0]
                    .credential_tier
                    .map(workforce_common::subscription::CredentialTier::as_str),
                Some(paid_by.as_str())
            );
        }
    }

    #[tokio::test]
    async fn coding_round_cost_names_login_owner_not_sentinel() {
        let tmp = tempfile::tempdir().unwrap();
        let service = service_in(tmp.path(), |_| {}).await;
        service
            .global_vault
            .as_ref()
            .expect("a global vault")
            .set(
                &crate::user_config::per_user_key(ALICE, SubscriptionKind::Claude.vault_key()),
                &unique("alice"),
            )
            .unwrap();
        let claude = ScriptedClaude::new();
        let heartbeat = workforce_common::session::TurnPrincipal {
            run_as: Some("heartbeat".into()),
            bill_to: Some(ALICE.into()),
        };

        round_as(&service, &claude, &heartbeat).await.unwrap();

        let rows = super::super::tests::cost_rows(&service);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].payer.as_deref(),
            Some(ALICE),
            "the plan that paid is Alice's, not the sentinel the round ran as"
        );
    }

    #[tokio::test]
    async fn coding_round_throttle_cools_login_for_turns() {
        let tmp = tempfile::tempdir().unwrap();
        let service = service_in(tmp.path(), |_| {}).await;
        service
            .vault
            .as_ref()
            .unwrap()
            .set(SubscriptionKind::Claude.vault_key(), &unique("shared"))
            .unwrap();
        let claude = ScriptedClaude::new();
        claude.reply(
            None,
            &[serde_json::json!({
                "type": "result",
                "subtype": "success",
                "is_error": true,
                "num_turns": 1,
                "result": "API Error: Request rejected (429) · rate limited",
            })],
        );

        let throttled = round_as(&service, &claude, &alice()).await.unwrap();

        assert_eq!(throttled.completion, crate::claude_code::Completion::Failed);
        let no_login = crate::user_config::as_principal(&alice(), async {
            service.logins.resolve(SubscriptionKind::Claude)
        })
        .await
        .expect_err("the throttled login is skipped");
        assert!(
            no_login.cooling_until().is_some(),
            "an agent turn now skips it until it cools: {no_login}"
        );
        let refused = crate::user_config::as_principal(&alice(), async {
            ClaudeCodeEngine.preflight(&context(&service))
        })
        .await
        .unwrap_err();
        assert_eq!(
            refused.trigger,
            workforce_common::settings::FallbackTrigger::RateLimited,
            "a chain moves past the cooling login as past a limit"
        );
    }

    #[tokio::test]
    async fn coding_round_without_login_gets_connect_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let service = service_in(tmp.path(), |_| {}).await;
        let claude = ScriptedClaude::new();

        let refused = crate::user_config::as_principal(&alice(), async {
            ClaudeCodeEngine.preflight(&context(&service))
        })
        .await
        .unwrap_err();
        let Err(failed) = round_as(&service, &claude, &alice()).await else {
            panic!("a round with no login runs");
        };

        for notice in [refused.reason, failed.to_string()] {
            assert!(notice.contains("claude setup-token"), "{notice}");
            assert!(notice.contains("My Settings"), "{notice}");
        }
        assert_eq!(claude.runs(), 0, "nothing ran without a login");
    }

    #[tokio::test]
    async fn claude_round_cost_prices_tokens_not_cumulative_total() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = workforce_common::settings::WorkforceConfig::default();
        cfg.hub.data_dir = tmp.path().to_string_lossy().to_string();
        cfg.memory.db_path = std::path::Path::new(&cfg.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        cfg.llm.api_key = Some("test-key".into());
        let service = crate::serve::AgentService::from_config(cfg, None)
            .await
            .unwrap();
        service
            .vault
            .clone()
            .expect("agent vault")
            .set(
                workforce_common::subscription::SubscriptionKind::Claude.vault_key(),
                "agent-plan",
            )
            .unwrap();
        let resumed = tmp.path().join("claude");
        let result_event = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "num_turns": 1,
            "result": "done",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 117,
                "cache_creation_input_tokens": 192,
                "cache_read_input_tokens": 8326,
            },
            "total_cost_usd": 9.5,
        });
        std::fs::write(
            &resumed,
            format!("#!/bin/sh\ncat > /dev/null\nprintf '%s\\n' '{result_event}'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&resumed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ctx = RoundContext {
            service: &service,
            identity: None,
            stand_in: workforce_common::subscription::SCHEDULER,
            ticket_id: None,
            description: "coding.session",
            resume: crate::mcp_gate::RouteResume::Nowhere,
        };
        let round = CodingRound {
            model: Some("sonnet".into()),
            ..CodingRound::one_shot("fix it".into(), Duration::from_secs(60))
        };
        let sandbox =
            crate::coding_sandbox::adopt_workdir(tmp.path().to_path_buf(), Vec::new(), None);

        let result = crate::claude_code::tests::CLAUDE
            .scope(
                resumed,
                ClaudeCodeEngine.run(sandbox, &round, &ctx, None, None),
            )
            .await
            .unwrap();

        let priced = service
            .cost_tracker
            .calculate_cost("claude-code:sonnet", 10, 117, 192, 8326);
        assert!(
            (result.cost_usd - priced).abs() < 1e-12,
            "a resumed round's cost is its own tokens', not the transcript's 9.5: {}",
            result.cost_usd
        );
    }
}
