//! A person's own route end to end: on an agent whose own route is ChatGPT,
//! a call billed to someone who chose their own Claude runs on their own
//! Claude login, and on nothing anyone shares.

use std::sync::Arc;

use workforce_agent::llm::types::LlmMessage;
use workforce_agent::runner::{with_served_log, CURRENT_USER_ID};
use workforce_common::personal_route::{
    AgentRoute, PersonalRoute, PersonalRouteKind, PersonalRoutesConfig, RouteFallback,
};
use workforce_common::settings::WorkforceConfig;
use workforce_common::subscription::{CredentialTier, SubscriptionKind};

use crate::agent_turn::tests::{hub, Hub, ALICE};
use crate::claude_code::tests::scripted::{answer, result};
use crate::hub_core::personal_routes::{MemoryRouteStore, PersonRouteStore};
use crate::subscription::test_support::SKY;

const BOB: &str = "0b7d3c1e-5f2a-4c8b-9d6e-1a2b3c4d5e6f";

fn claude(fallback: RouteFallback) -> PersonalRoute {
    PersonalRoute {
        kind: PersonalRouteKind::ClaudeCode,
        fallback,
    }
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
