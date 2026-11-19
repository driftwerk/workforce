//! A person's own route end to end: on an agent whose own route is ChatGPT,
//! a call billed to someone who chose their own Claude runs on their own
//! Claude login, and on nothing anyone shares.

use std::sync::Arc;

use serde_json::json;

use workforce_agent::llm::types::LlmMessage;
use workforce_agent::runner::{with_served_log, CURRENT_USER_ID};
use workforce_common::personal_route::{
    AgentRoute, PersonalRoute, PersonalRouteKind, PersonalRoutesConfig, RouteFallback,
};
use workforce_common::settings::WorkforceConfig;
use workforce_common::subscription::{CredentialTier, SubscriptionKind};

use crate::agent_turn::tests::{hub, Hub, ALICE};
use crate::claude_code::tests::scripted::{answer, assistant, print, rejected, result};
use crate::hub_core::personal_routes::{MemoryRouteStore, PersonRouteStore};
use crate::subscription::test_support::SKY;

const BOB: &str = "0b7d3c1e-5f2a-4c8b-9d6e-1a2b3c4d5e6f";

fn claude(fallback: RouteFallback) -> PersonalRoute {
    PersonalRoute::new(PersonalRouteKind::ClaudeCode, fallback)
}

fn switch_on(config: &mut WorkforceConfig) {
    config.llm.personal_routes.enabled = true;
}

/// A hub whose agent runs on ChatGPT, with people's routes on and kept.
async fn routed() -> (Hub, Arc<MemoryRouteStore>) {
    let hub = hub(switch_on).await;
    let store = Arc::new(MemoryRouteStore::default());
    hub.service
        .subscriptions()
        .personal_routes()
        .attach_store(Arc::clone(&store) as Arc<dyn PersonRouteStore>);
    (hub, store)
}

async fn choose(hub: &Hub, store: &MemoryRouteStore, person: &str, route: Option<PersonalRoute>) {
    let person = person.parse().unwrap();
    store.set_every_agent(person, route, "test").await.unwrap();
    hub.service.subscriptions().personal_routes().forget(person);
}

async fn choose_here(hub: &Hub, store: &MemoryRouteStore, person: &str, route: AgentRoute) {
    let person = person.parse().unwrap();
    store
        .set_for_agent(person, hub.service.agent_id.as_str(), Some(route), "test")
        .await
        .unwrap();
    hub.service.subscriptions().personal_routes().forget(person);
}

/// A Claude login the agent shares with everyone.
fn shared_claude_login(hub: &Hub) {
    hub.service
        .vault
        .as_ref()
        .unwrap()
        .set(SubscriptionKind::Claude.vault_key(), "shared-token")
        .unwrap();
}

#[tokio::test]
async fn her_turn_runs_on_her_own_claude_login_at_the_agents_tier() {
    let (hub, store) = routed().await;
    let token = hub.claude_login(ALICE);
    shared_claude_login(&hub);
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;
    hub.claude.reply(None, &answer("Claude answers."));

    let reply = hub.ask(ALICE, "hello there").await.unwrap();

    assert_eq!(reply, "Claude answers.");
    let calls = hub.claude.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].token, token, "her own login, not the shared one");
    let model = calls[0].flag("--model").map(str::to_string);
    assert!(
        matches!(model.as_deref(), Some("opus" | "sonnet" | "haiku")),
        "the agent's model mapped to its Claude tier: {model:?}"
    );
    assert_eq!(
        hub.chatgpt_requests().await,
        0,
        "never the agent's own route"
    );
}

#[tokio::test]
async fn a_job_run_as_her_runs_on_her_login_and_its_cost_row_says_so() {
    let (hub, store) = routed().await;
    let token = hub.claude_login(ALICE);
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;
    hub.claude.reply(None, &answer("Digest."));

    hub.service
        .process_message(
            "scheduler",
            "scheduler",
            "nightly digest",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("personal-job-1"),
            None,
            Some(ALICE),
        )
        .await
        .unwrap();

    assert_eq!(hub.claude.calls()[0].token, token);
    let trace = hub
        .service
        .cost_tracker
        .telemetry_store()
        .unwrap()
        .get_task_trace("personal-job-1")
        .unwrap()
        .expect("the job's trace");
    let row = trace
        .cost_records
        .iter()
        .find(|row| row.model.starts_with("claude-code:"))
        .unwrap_or_else(|| panic!("a claude-code row in {:?}", trace.cost_records));
    assert_eq!(
        row.billing,
        workforce_common::subscription::Billing::Subscription
    );
    assert_eq!(row.payer.as_deref(), Some(ALICE));
    assert_eq!(row.credential_tier, Some(CredentialTier::AgentUser));
}

#[tokio::test]
async fn the_per_agent_setting_beats_every_agent_and_hub_keeps_the_agents_route() {
    let (hub, store) = routed().await;
    hub.claude_login(ALICE);
    hub.chatgpt_login(ALICE).await;
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;
    choose_here(&hub, &store, ALICE, AgentRoute::Hub).await;

    assert_eq!(hub.ask(ALICE, "why is the sky blue?").await.unwrap(), SKY);
    assert!(hub.claude.calls().is_empty());

    choose(&hub, &store, ALICE, None).await;
    choose_here(
        &hub,
        &store,
        ALICE,
        AgentRoute::Personal(claude(RouteFallback::Wait)),
    )
    .await;
    hub.claude.reply(None, &answer("Claude answers."));
    assert_eq!(hub.ask(ALICE, "hello").await.unwrap(), "Claude answers.");
}

#[tokio::test]
async fn someone_elses_turn_never_uses_her_login() {
    let (hub, store) = routed().await;
    hub.claude_login(ALICE);
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;
    hub.chatgpt_login(BOB).await;

    assert_eq!(hub.ask(BOB, "why is the sky blue?").await.unwrap(), SKY);
    assert!(hub.claude.calls().is_empty());
}

#[tokio::test]
async fn a_turn_with_no_person_stays_on_the_agents_route() {
    let (hub, store) = routed().await;
    hub.claude_login(ALICE);
    shared_claude_login(&hub);
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Hub))).await;

    let reply = hub.ask("scheduler", "nightly digest").await;

    assert!(hub.claude.calls().is_empty(), "{reply:?}");
    assert!(
        hub.chatgpt_requests().await > 0 || reply.is_err(),
        "it tried the agent's own route: {reply:?}"
    );
}

#[tokio::test]
async fn without_her_login_wait_stops_and_never_draws_on_a_shared_one() {
    let (hub, store) = routed().await;
    shared_claude_login(&hub);
    hub.chatgpt_login(ALICE).await;
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;

    let reply = hub.ask(ALICE, "hello").await.unwrap();

    assert!(
        reply.contains("connect your Claude plan"),
        "she's told to connect her own: {reply}"
    );
    assert!(hub.claude.calls().is_empty(), "not on the shared login");
    assert_eq!(hub.chatgpt_requests().await, 0, "she chose to wait");
}

#[tokio::test]
async fn without_her_login_hub_falls_back_to_the_agents_route() {
    let (hub, store) = routed().await;
    shared_claude_login(&hub);
    hub.chatgpt_login(ALICE).await;
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Hub))).await;

    assert_eq!(hub.ask(ALICE, "why is the sky blue?").await.unwrap(), SKY);
    assert!(hub.claude.calls().is_empty(), "not on the shared login");
}

#[tokio::test]
async fn with_the_switch_off_or_the_kind_not_allowed_her_turn_stays_on_the_agents_route() {
    let (hub, store) = routed().await;
    hub.claude_login(ALICE);
    hub.chatgpt_login(ALICE).await;
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;
    let routes = hub.service.subscriptions().personal_routes();

    routes.set_switch(PersonalRoutesConfig::default());
    assert_eq!(hub.ask(ALICE, "why is the sky blue?").await.unwrap(), SKY);

    routes.set_switch(PersonalRoutesConfig {
        enabled: true,
        allow: vec![PersonalRouteKind::Codex],
    });
    assert_eq!(hub.ask(ALICE, "why is the sky blue?").await.unwrap(), SKY);

    let mut resolved = (*hub.service.resolved.snapshot()).clone();
    resolved.personal_routes = Some(workforce_common::personal_route::AgentPersonalRoutes {
        enabled: Some(false),
        allow: None,
    });
    hub.service.resolved.store(resolved);
    routes.set_switch(PersonalRoutesConfig {
        enabled: true,
        ..Default::default()
    });
    assert_eq!(hub.ask(ALICE, "why is the sky blue?").await.unwrap(), SKY);

    assert!(hub.claude.calls().is_empty());
}

#[tokio::test]
async fn her_background_call_runs_as_a_one_shot_on_her_own_login() {
    let (hub, store) = routed().await;
    let token = hub.claude_login(ALICE);
    shared_claude_login(&hub);
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;
    hub.claude.reply(None, &[result("A summary.")]);
    let llm = Arc::clone(&hub.service.llm);
    let messages = [LlmMessage::user("Summarise this conversation.")];

    let (answer, served) = with_served_log(
        CURRENT_USER_ID.scope(
            ALICE.to_string(),
            hub.claude
                .scoped(llm.create_message(None, &messages, &[], None, None, false)),
        ),
    )
    .await;

    assert_eq!(answer.unwrap().text().as_deref(), Some("A summary."));
    assert_eq!(hub.claude.calls()[0].token, token);
    let served = served.expect("what paid is noted for the cost row");
    assert_eq!(served.tier, CredentialTier::AgentUser);
    assert_eq!(hub.chatgpt_requests().await, 0);
}

#[tokio::test]
async fn a_call_run_as_her_but_billed_to_someone_else_stays_on_the_agents_route() {
    let (hub, store) = routed().await;
    hub.claude_login(ALICE);
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;
    hub.chatgpt_login(ALICE).await;

    let reply = workforce_agent::runner::CURRENT_BILLING_IDENTITY
        .scope(BOB.to_string(), hub.ask(ALICE, "why is the sky blue?"))
        .await;

    assert!(hub.claude.calls().is_empty(), "not her route: {reply:?}");
}

#[tokio::test]
async fn an_isolated_run_for_her_runs_on_her_login() {
    let (hub, store) = routed().await;
    let token = hub.claude_login(ALICE);
    shared_claude_login(&hub);
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;
    hub.claude.reply(None, &answer("Step done."));

    // As a workflow step or subagent runs: spawned, so nothing of the
    // caller's scope is left but the principal it passes.
    let run = crate::serve::IsolatedRun {
        principal: workforce_common::session::TurnPrincipal {
            run_as: Some(ALICE.into()),
            bill_to: None,
        },
        route: crate::serve::IsolatedRoute::Agent,
    };
    let result = hub
        .service
        .run_isolated(
            "workflow",
            "summarise the step",
            None,
            None,
            &run,
            crate::job_runner::AgentProfile::Worker,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(result.response, "Step done.");
    let calls = hub.claude.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].token, token,
        "a workflow step runs on her own login"
    );
}

#[tokio::test]
async fn at_her_plans_limit_wait_stops_and_a_turn_while_it_cools_spawns_nothing() {
    let (hub, store) = routed().await;
    hub.claude_login(ALICE);
    shared_claude_login(&hub);
    hub.chatgpt_login(ALICE).await;
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;
    hub.claude.reply(None, &rejected());

    let err = hub.ask(ALICE, "hello").await.unwrap_err().to_string();

    assert!(
        err.contains("is at its usage limit until"),
        "the turn stops and says her plan is at its limit: {err}"
    );
    assert_eq!(hub.claude.calls().len(), 1, "once, on her login");
    assert_eq!(hub.chatgpt_requests().await, 0, "she chose to wait");

    let again = hub.ask(ALICE, "hello again").await.unwrap_err().to_string();
    assert!(again.contains("at its usage limit until"), "{again}");
    assert_eq!(
        hub.claude.calls().len(),
        1,
        "a cooling login spawns no claude"
    );
    assert_eq!(
        hub.chatgpt_requests().await,
        0,
        "nor reaches the hub's route"
    );
}

#[tokio::test]
async fn at_her_plans_limit_hub_answers_on_the_agents_route() {
    let (hub, store) = routed().await;
    hub.claude_login(ALICE);
    hub.chatgpt_login(ALICE).await;
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Hub))).await;
    hub.claude.reply(None, &rejected());

    assert_eq!(hub.ask(ALICE, "why is the sky blue?").await.unwrap(), SKY);
    assert_eq!(hub.claude.calls().len(), 1);
    assert_eq!(hub.chatgpt_requests().await, 1);
}

#[tokio::test]
async fn hub_hands_over_a_failure_before_output_but_never_one_after() {
    let (hub, store) = routed().await;
    hub.claude_login(ALICE);
    hub.chatgpt_login(ALICE).await;
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Hub))).await;
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
        "her route produced output, so it is never handed over"
    );

    hub.claude.script(None, "exit 1\n");
    assert_eq!(hub.ask(ALICE, "why is the sky blue?").await.unwrap(), SKY);
    assert_eq!(hub.claude.calls().len(), 2);
    assert_eq!(hub.chatgpt_requests().await, 1, "handed over before output");
}

#[tokio::test]
async fn her_compact_command_summarises_on_her_own_login() {
    let (hub, store) = routed().await;
    let token = hub.claude_login(ALICE);
    shared_claude_login(&hub);
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;
    let mut session = hub
        .service
        .sessions
        .get_or_create("web", ALICE, hub.service.agent_id.as_str())
        .unwrap();
    for n in 0..30 {
        session.add_user_message(&format!("Question {n} about the deploy."), None);
        session.add_assistant_message(&format!("Answer {n}: it rolled out fine."), 40);
    }
    hub.service.sessions.save(&session).unwrap();
    hub.claude.reply(None, &[result("The summary.")]);

    let reply = hub.claude.scoped(hub.ask(ALICE, "/compact")).await.unwrap();

    assert!(reply.contains("Compacted"), "{reply}");
    let calls = hub.claude.calls();
    assert!(!calls.is_empty(), "the summary ran on claude");
    for call in &calls {
        assert_eq!(call.token, token, "her own login, never the shared one");
    }
    assert_eq!(hub.chatgpt_requests().await, 0);
}

#[tokio::test]
async fn a_knowledge_summary_made_while_she_is_in_scope_is_the_hubs_own_upkeep() {
    use workforce_knowledge_service::SummariserBuilder as _;

    let (hub, store) = routed().await;
    let token = hub.claude_login(ALICE);
    shared_claude_login(&hub);
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;
    let mut config = WorkforceConfig::default();
    config.hub.data_dir = hub.dir.path().display().to_string();
    let plans = crate::subscription::PlanLogins::new(
        crate::subscription::Subscriptions::new(&config),
        crate::subscription::CredentialScope::system(
            Arc::clone(hub.service.vault.as_ref().unwrap()),
            &config.subscription,
        ),
    );
    let summariser = crate::knowledge::HubSummariserBuilder::new(
        workforce_common::settings::KnowledgeSummariserConfig {
            provider: "claude-code".into(),
            model: "claude-code:cheap".into(),
            ..Default::default()
        },
    )
    .with_plans(Some(plans))
    .build()
    .unwrap();
    hub.claude.reply(None, &answer("A module summary."));

    // A knowledge tool call she made starts the pass.
    let summary = CURRENT_USER_ID
        .scope(
            ALICE.to_string(),
            hub.claude
                .scoped(summariser.generate_text("Summarise this module.", 200)),
        )
        .await
        .unwrap();

    assert_eq!(summary, "A module summary.");
    let calls = hub.claude.calls();
    assert_eq!(calls.len(), 1);
    assert_ne!(calls[0].token, token, "never her login");
    assert_eq!(calls[0].token, "shared-token", "the hub's own");
}

/// The `claude_code` tool called as `person`, as a call that came back
/// through the gated MCP restores them: none of the turn's scope but its
/// principal.
async fn claude_code_as(hub: &Hub, person: &str) -> workforce_agent::tools::ToolResult {
    use workforce_agent::tools::Tool;
    let workdir = hub.dir.path().to_path_buf();
    let tool = crate::claude_code_tool::ClaudeCodeTool::new(
        Arc::clone(&hub.service),
        vec![workdir.clone()],
    );
    let input = serde_json::json!({
        "task": "fix the bug",
        "workdir": workdir.to_string_lossy(),
    });
    crate::user_config::as_principal(
        &workforce_common::session::TurnPrincipal::run_as(Some(person.to_string())),
        hub.claude.scoped(tool.execute(input)),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn her_claude_code_call_waits_for_her_own_login_never_a_shared_one() {
    let (hub, store) = routed().await;
    shared_claude_login(&hub);
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;

    let refused = claude_code_as(&hub, ALICE).await;

    assert!(!refused.success);
    assert!(
        refused
            .error
            .as_deref()
            .is_some_and(|error| error.contains("connect your Claude plan")),
        "she's told to connect her own: {:?}",
        refused.error
    );
    assert!(hub.claude.calls().is_empty(), "not on the shared login");

    let token = hub.claude_login(ALICE);
    hub.claude.reply(None, &answer("Fixed."));
    claude_code_as(&hub, ALICE).await;

    let calls = hub.claude.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].token, token, "her own login");
}

#[tokio::test]
async fn her_claude_code_call_may_draw_on_a_shared_login_when_she_chose_hub_or_no_route() {
    let (hub, store) = routed().await;
    shared_claude_login(&hub);
    hub.claude.reply(None, &answer("Fixed."));

    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Hub))).await;
    claude_code_as(&hub, ALICE).await;
    choose(&hub, &store, ALICE, None).await;
    claude_code_as(&hub, ALICE).await;

    let tokens: Vec<String> = hub.claude.calls().into_iter().map(|c| c.token).collect();
    assert_eq!(
        tokens,
        vec!["shared-token".to_string(); 2],
        "her fallback to the hub, and no route at all, keep today's logins"
    );
}

/// `work` as a coding round for Alice decides its logins: her principal
/// restored, nothing else of a turn's scope.
async fn as_alice<F: std::future::Future>(
    service: &crate::serve::AgentService,
    work: F,
) -> F::Output {
    crate::user_config::as_principal(
        &workforce_common::session::TurnPrincipal::run_as(Some(ALICE.to_string())),
        super::own_logins_if_routed(service, work),
    )
    .await
}

#[tokio::test]
async fn a_coding_round_and_a_collected_one_for_her_draw_on_her_own_logins_only() {
    let (hub, store) = routed().await;
    shared_claude_login(&hub);
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;
    let service = &hub.service;
    let ctx = crate::coding_engine::RoundContext {
        service,
        identity: Some(ALICE),
        stand_in: workforce_common::subscription::SCHEDULER,
        ticket_id: None,
        description: "coding.session",
        resume: crate::mcp_gate::RouteResume::Nowhere,
    };

    let refused = as_alice(service, async {
        ctx.login(SubscriptionKind::Claude).map(drop)
    })
    .await
    .expect_err("no shared login for her round");
    assert!(
        refused.reason.contains("connect your Claude plan"),
        "{}",
        refused.reason
    );
    let held = as_alice(service, async {
        service
            .logins
            .holding(SubscriptionKind::Claude, &|_| true)
            .map(drop)
    })
    .await;
    assert!(
        held.is_none(),
        "a collected round never goes on with a shared login"
    );

    choose(&hub, &store, ALICE, None).await;
    assert!(as_alice(service, async {
        ctx.login(SubscriptionKind::Claude).map(drop)
    })
    .await
    .is_ok());
}

// ── Codex ───────────────────────────────────────────────────────────────

fn codex(fallback: RouteFallback) -> PersonalRoute {
    PersonalRoute::new(PersonalRouteKind::Codex, fallback)
}

/// A hub whose agent runs on ChatGPT, with its coding worker (where Codex
/// runs), people's routes on and kept.
async fn routed_on_worker() -> (Hub, Arc<MemoryRouteStore>) {
    let hub = crate::agent_turn::tests::codex_hub(|config| {
        switch_on(config);
        config.coding_session.executor.kind = workforce_common::settings::ExecutorKind::Remote;
    })
    .await;
    let store = Arc::new(MemoryRouteStore::default());
    hub.service
        .subscriptions()
        .personal_routes()
        .attach_store(Arc::clone(&store) as Arc<dyn PersonRouteStore>);
    (hub, store)
}

/// `person`'s own ChatGPT login, kept where the hub's own ChatGPT route
/// would never be asked for it; returns its access token.
fn chatgpt_login_of(hub: &Hub, person: &str) -> String {
    crate::serve::testing::store_chatgpt_login(&hub.service, person)
}

/// A ChatGPT login the agent shares with everyone; returns its access token.
fn shared_chatgpt_login(hub: &Hub) -> String {
    let access = format!("shared-{}", uuid::Uuid::new_v4());
    hub.service
        .vault
        .as_ref()
        .unwrap()
        .set(
            SubscriptionKind::ChatGpt.vault_key(),
            &crate::subscription::test_support::login(
                &format!("acct-{}", uuid::Uuid::new_v4()),
                &access,
                "rt-0",
            ),
        )
        .unwrap();
    access
}

/// The access token Codex run `n` was handed.
fn codex_access(hub: &Hub, n: usize) -> String {
    crate::subscription::AuthJson::parse(&hub.codex.recorded(n, "auth"))
        .unwrap()
        .access_token()
        .unwrap()
        .to_string()
}

#[test]
fn every_tier_maps_to_the_one_codex_plan_model() {
    use crate::hub_core::subscriptions::CliKind;
    let tiers = workforce_agent::llm::tiers::TierRegistry::new(None);
    for spec in [
        "cheap",
        "standard",
        "powerful",
        "anthropic:powerful",
        "claude-opus-4-8",
    ] {
        assert_eq!(
            super::personal_model(
                &tiers,
                "anthropic",
                spec,
                CliKind::Codex,
                &codex(RouteFallback::Wait),
                workforce_common::personal_route::RouteWork::Other,
            ),
            crate::subscription::completion::CODEX_PLAN_MODEL,
            "{spec}"
        );
    }
}

#[tokio::test]
async fn her_turn_runs_on_her_own_codex_login_on_the_worker() {
    let (hub, store) = routed_on_worker().await;
    let access = chatgpt_login_of(&hub, ALICE);
    shared_chatgpt_login(&hub);
    choose(&hub, &store, ALICE, Some(codex(RouteFallback::Wait))).await;

    let (_, result) = hub.stream(ALICE, "hello there", None, None).await;

    let result = result.unwrap();
    assert_eq!(
        result.response,
        crate::coding_engine::codex::tests::SCRIPTED_ANSWER
    );
    assert_eq!(result.model, "codex:gpt-6.1-sol");
    assert_eq!(hub.codex.runs(), 1);
    assert_eq!(
        codex_access(&hub, 1),
        access,
        "her own login, not the shared one"
    );
    let config: toml::Table = hub.codex.recorded(1, "config").parse().unwrap();
    assert_eq!(config["model"].as_str(), Some("gpt-6.1-sol"));
    assert_eq!(
        hub.chatgpt_requests().await,
        0,
        "never the agent's own route"
    );
    assert!(hub.claude.calls().is_empty());
}

#[tokio::test]
async fn without_her_codex_login_wait_stops_and_never_draws_on_a_shared_one() {
    let (hub, store) = routed_on_worker().await;
    shared_chatgpt_login(&hub);
    choose(&hub, &store, ALICE, Some(codex(RouteFallback::Wait))).await;

    let reply = hub.ask(ALICE, "hello").await;

    assert_eq!(hub.codex.runs(), 0, "not on the shared login: {reply:?}");
    assert_eq!(hub.chatgpt_requests().await, 0, "she chose to wait");
}

#[tokio::test]
async fn someone_elses_turn_never_uses_her_codex_login() {
    let (hub, store) = routed_on_worker().await;
    chatgpt_login_of(&hub, ALICE);
    choose(&hub, &store, ALICE, Some(codex(RouteFallback::Wait))).await;
    hub.chatgpt_login(BOB).await;

    assert_eq!(hub.ask(BOB, "why is the sky blue?").await.unwrap(), SKY);
    assert_eq!(hub.codex.runs(), 0);
}

#[tokio::test]
async fn off_the_worker_wait_refuses_her_codex_route_with_a_plain_reason() {
    let (hub, store) = routed().await;
    chatgpt_login_of(&hub, ALICE);
    choose(&hub, &store, ALICE, Some(codex(RouteFallback::Wait))).await;

    let err = hub.ask(ALICE, "hello").await.unwrap_err();

    assert!(
        format!("{err:#}").contains(super::CODEX_NEEDS_WORKER),
        "{err:#}"
    );
    assert_eq!(hub.codex.runs(), 0);
    assert_eq!(hub.chatgpt_requests().await, 0, "she chose to wait");
}

#[tokio::test]
async fn off_the_worker_hub_runs_her_turn_on_the_agents_route() {
    let (hub, store) = routed().await;
    hub.chatgpt_login(ALICE).await;
    choose(&hub, &store, ALICE, Some(codex(RouteFallback::Hub))).await;

    assert_eq!(hub.ask(ALICE, "why is the sky blue?").await.unwrap(), SKY);
    assert_eq!(hub.codex.runs(), 0);
}

#[tokio::test]
async fn her_background_call_runs_as_a_codex_one_shot_on_her_own_login() {
    let (hub, store) = routed_on_worker().await;
    let access = chatgpt_login_of(&hub, ALICE);
    shared_chatgpt_login(&hub);
    choose(&hub, &store, ALICE, Some(codex(RouteFallback::Wait))).await;
    let llm = Arc::clone(&hub.service.llm);
    let messages = [LlmMessage::user("Summarise this conversation.")];

    let (answer, served) = with_served_log(CURRENT_USER_ID.scope(
        ALICE.to_string(),
        crate::coding_engine::codex::tests::CODEX.scope(
            hub.codex.program(),
            llm.create_message(None, &messages, &[], None, None, false),
        ),
    ))
    .await;

    assert_eq!(
        answer.unwrap().text().as_deref(),
        Some(crate::coding_engine::codex::tests::SCRIPTED_ANSWER)
    );
    assert_eq!(hub.codex.runs(), 1);
    assert_eq!(codex_access(&hub, 1), access);
    let config: toml::Table = hub.codex.recorded(1, "config").parse().unwrap();
    assert_eq!(config["model"].as_str(), Some("gpt-6.1-sol"));
    let served = served.expect("what paid is noted for the cost row");
    assert_eq!(served.tier, CredentialTier::AgentUser);
    assert_eq!(
        served.route,
        workforce_common::subscription::SubscriptionRoute::Codex
    );
    assert_eq!(hub.chatgpt_requests().await, 0);
}

// ── The models a person chose for their route ───────────────────────────

/// `route` with `conversation`, `light` and `heavy` chosen.
fn with_models(
    mut route: PersonalRoute,
    conversation: Option<&str>,
    light: Option<&str>,
    heavy: Option<&str>,
) -> PersonalRoute {
    if let Some(m) = conversation {
        route
            .model_preferences
            .insert("conversation".into(), m.into());
    }
    route.light_model = light.map(str::to_string);
    route.heavy_model = heavy.map(str::to_string);
    route
}

#[test]
fn each_kind_of_work_runs_the_model_she_chose_else_the_routes_default() {
    use crate::hub_core::subscriptions::CliKind;
    use workforce_common::personal_route::RouteWork;
    let tiers = workforce_agent::llm::tiers::TierRegistry::new(None);
    let model = |cli, route: &PersonalRoute, work| {
        super::personal_model(&tiers, "anthropic", "standard", cli, route, work)
    };

    let on_claude = with_models(
        claude(RouteFallback::Wait),
        Some("haiku"),
        Some("powerful"),
        Some("chat"),
    );
    assert_eq!(
        model(CliKind::ClaudeCode, &on_claude, RouteWork::Conversation),
        "haiku"
    );
    assert_eq!(
        model(CliKind::ClaudeCode, &on_claude, RouteWork::Light),
        "opus",
        "a tier is Claude's model at it"
    );
    assert_eq!(
        model(CliKind::ClaudeCode, &on_claude, RouteWork::Heavy),
        "haiku",
        "chat is the conversation model"
    );
    assert_eq!(
        model(CliKind::ClaudeCode, &on_claude, RouteWork::Other),
        "sonnet",
        "a job's turn keeps the agent's tier"
    );
    assert_eq!(
        model(
            CliKind::ClaudeCode,
            &claude(RouteFallback::Wait),
            RouteWork::Conversation
        ),
        "sonnet",
        "nothing chosen is the agent's tier"
    );

    let on_codex = with_models(
        codex(RouteFallback::Wait),
        Some("gpt-6-astra"),
        Some("gpt-6-luna"),
        Some("cheap"),
    );
    assert_eq!(
        model(CliKind::Codex, &on_codex, RouteWork::Conversation),
        "gpt-6-astra"
    );
    assert_eq!(
        model(CliKind::Codex, &on_codex, RouteWork::Light),
        "gpt-6-luna"
    );
    let plan = crate::subscription::completion::CODEX_PLAN_MODEL;
    assert_eq!(
        model(CliKind::Codex, &on_codex, RouteWork::Heavy),
        plan,
        "a tier on Codex is the one plan model"
    );
    assert_eq!(model(CliKind::Codex, &on_codex, RouteWork::Other), plan);
}

#[tokio::test]
async fn her_chat_turn_runs_the_conversation_model_she_chose_and_a_job_its_tier() {
    let (hub, store) = routed().await;
    hub.claude_login(ALICE);
    let route = with_models(claude(RouteFallback::Wait), Some("haiku"), None, None);
    choose(&hub, &store, ALICE, Some(route)).await;
    hub.claude.reply(None, &answer("Claude answers."));

    crate::serve::chat_turn(hub.ask(ALICE, "hello there"))
        .await
        .unwrap();
    assert_eq!(hub.claude.calls()[0].flag("--model"), Some("haiku"));

    hub.claude.reply(None, &answer("Digest."));
    hub.ask(ALICE, "nightly digest").await.unwrap();
    let job = hub.claude.calls()[1].flag("--model").map(str::to_string);
    assert!(
        matches!(job.as_deref(), Some("opus" | "sonnet")),
        "a turn no message of hers started keeps the agent's tier: {job:?}"
    );
}

#[tokio::test]
async fn her_background_calls_run_the_light_and_heavy_codex_models_she_chose() {
    let (hub, store) = routed_on_worker().await;
    chatgpt_login_of(&hub, ALICE);
    let route = with_models(
        codex(RouteFallback::Wait),
        None,
        Some("gpt-6-luna"),
        Some("gpt-6-astra"),
    );
    choose(&hub, &store, ALICE, Some(route)).await;
    let light = hub
        .service
        .resolved
        .snapshot()
        .background
        .light
        .clone()
        .expect("the agent's light model");
    let llm = Arc::clone(&hub.service.llm);
    let messages = [LlmMessage::user("Summarise this conversation.")];
    let call = |model: Option<String>| {
        let llm = Arc::clone(&llm);
        let messages = messages.clone();
        let program = hub.codex.program();
        async move {
            CURRENT_USER_ID
                .scope(
                    ALICE.to_string(),
                    crate::coding_engine::codex::tests::CODEX.scope(
                        program,
                        llm.create_message(None, &messages, &[], model.as_deref(), None, false),
                    ),
                )
                .await
                .unwrap()
        }
    };

    call(Some(light)).await;
    call(None).await;

    let model = |n| {
        let config: toml::Table = hub.codex.recorded(n, "config").parse().unwrap();
        config["model"].as_str().unwrap().to_string()
    };
    assert_eq!(model(1), "gpt-6-luna", "light work on her light model");
    assert_eq!(
        model(2),
        "gpt-6-astra",
        "anything else billed to her is heavy"
    );
    assert_eq!(hub.chatgpt_requests().await, 0);
}

/// What `person`'s own conversations with the agent are shown to run on.
async fn shown(hub: &Hub, person: &str) -> Option<workforce_gateway_client::PersonalRouteInfo> {
    crate::api_handlers::llm_route::shown_route(&hub.service, person.parse().unwrap()).await
}

#[tokio::test]
async fn her_desk_chat_turn_runs_on_her_claude_login_at_the_model_she_is_shown() {
    let (hub, store) = routed().await;
    let token = hub.claude_login(ALICE);
    shared_claude_login(&hub);
    let mut route = claude(RouteFallback::Wait);
    route
        .model_preferences
        .insert("conversation".into(), "claude-chat-test".into());
    choose(&hub, &store, ALICE, Some(route)).await;
    hub.claude.reply(None, &answer("Claude answers."));

    let shown = shown(&hub, ALICE).await.expect("her own route is shown");
    let (_, result) = crate::serve::chat_turn(hub.stream_at(
        ("desk", None, None),
        ALICE,
        "hello there",
        None,
        None,
    ))
    .await;

    result.unwrap();
    let calls = hub.claude.calls();
    assert_eq!(calls.len(), 1, "the turn ran on Claude Code");
    assert_eq!(calls[0].token, token, "her own login, not the shared one");
    assert_eq!(calls[0].flag("--model"), Some("claude-chat-test"));
    assert_eq!(
        shown.model, "claude-chat-test",
        "what she's shown is what ran"
    );
    assert_eq!(shown.kind, "claude-code");
    assert_eq!(shown.title, "Claude");
    assert_eq!(shown.fallback, "wait");
    assert!(shown.connected);
    assert_eq!(
        hub.chatgpt_requests().await,
        0,
        "never the agent's own route"
    );
}

#[tokio::test]
async fn with_no_model_chosen_she_is_shown_the_claude_model_her_turn_runs() {
    let (hub, store) = routed().await;
    hub.claude_login(ALICE);
    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Hub))).await;
    hub.claude.reply(None, &answer("Claude answers."));

    let shown = shown(&hub, ALICE).await.expect("her own route is shown");
    let (_, result) = crate::serve::chat_turn(hub.stream_at(
        ("desk", None, None),
        ALICE,
        "hello there",
        None,
        None,
    ))
    .await;

    result.unwrap();
    let calls = hub.claude.calls();
    assert_eq!(calls[0].flag("--model"), Some(shown.model.as_str()));
    assert_eq!(shown.fallback, "hub");
}

#[tokio::test]
async fn only_a_person_whose_turns_take_their_own_route_is_shown_one() {
    let (hub, store) = routed().await;
    assert_eq!(shown(&hub, ALICE).await, None, "the hub's route");

    choose(&hub, &store, ALICE, Some(claude(RouteFallback::Wait))).await;
    let without_login = shown(&hub, ALICE).await.expect("her route");
    assert!(
        !without_login.connected,
        "a turn of hers waits until she connects her own login"
    );
    assert_eq!(shown(&hub, BOB).await, None, "Bob's is his own");

    choose_here(&hub, &store, ALICE, AgentRoute::Hub).await;
    assert_eq!(
        shown(&hub, ALICE).await,
        None,
        "this agent stays on the hub's"
    );

    choose_here(
        &hub,
        &store,
        ALICE,
        AgentRoute::Personal(claude(RouteFallback::Hub)),
    )
    .await;
    let routes = hub.service.subscriptions().personal_routes();
    routes.set_switch(PersonalRoutesConfig::default());
    assert_eq!(shown(&hub, ALICE).await, None, "the hub's switch is off");
    routes.set_switch(PersonalRoutesConfig {
        enabled: true,
        allow: vec![PersonalRouteKind::Codex],
    });
    assert_eq!(shown(&hub, ALICE).await, None, "Claude isn't offered");
}
