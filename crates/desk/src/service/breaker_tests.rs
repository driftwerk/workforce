//! The breaker as sessions meet it: through their hooks, their spend, and
//! what others ask of them.

use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_core::budget::{BudgetScope, OnExceed, Period, Unit};
use workforce_postgres::test_support::with_test_pool;

use super::tests::{desk, new_session, post};
use super::{Desk, DeskError, NewBudget, Rules};
use crate::crew::{By, Party};
use crate::engine::Engine;
use crate::floor::{Answer, DecideRef};
use crate::ledger::{Billing, Priced, Spend, Tokens};
use crate::ledger_store::Kept;
use crate::model::NewSession;
use crate::store::build_pool;

fn tool(command: &str) -> Value {
    json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": command}})
}

fn failed(command: &str) -> Value {
    json!({"hook_event_name": "PostToolUseFailure", "tool_name": "Bash",
        "tool_input": {"command": command}, "tool_use_id": "t", "error": "exit 101"})
}

fn done() -> Value {
    json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": {"command": "ls"}})
}

fn prompt() -> Value {
    json!({"hook_event_name": "UserPromptSubmit", "prompt": "go on"})
}

fn stop() -> Value {
    json!({"hook_event_name": "Stop", "last_assistant_message": "Done."})
}

fn agent() -> By {
    By {
        party: Party::agent("atlas", "Atlas"),
        reason: None,
        suggestion_id: None,
        hop: 0,
    }
}

fn denied(v: &Value) -> bool {
    v.pointer("/hookSpecificOutput/permissionDecision") == Some(&json!("deny"))
}

/// Spend `usd` kept for `session` now.
async fn spent(d: &Desk, session: Uuid, usd: f64) {
    let spend = Spend {
        key: Uuid::new_v4().to_string(),
        at: Utc::now(),
        model: Some("claude-opus-5-5".into()),
        tokens: Tokens {
            input: 1000,
            output: 1000,
            ..Default::default()
        },
        usd: Some(usd),
        priced: Priced::Reported,
        billing: None,
    };
    d.0.store
        .ledger_write(
            session,
            Engine::Claude,
            None,
            &[Kept {
                spend,
                billing: Billing::Plan,
                account: None,
            }],
        )
        .await
        .unwrap();
}

fn budget(scope: BudgetScope, amount: f64) -> NewBudget {
    NewBudget {
        scope,
        group: None,
        period: Period::Day,
        unit: Unit::Usd,
        amount,
        warn_at: None,
        on_exceed: None,
    }
}

#[tokio::test]
async fn with_no_budget_and_loops_unwatched_nothing_a_session_does_meets_the_breaker() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        post(&d, s.id, prompt()).await;
        for _ in 0..20 {
            assert_eq!(post(&d, s.id, tool("cargo test")).await, json!({}));
            assert_eq!(post(&d, s.id, failed("cargo test")).await, json!({}));
            assert_eq!(post(&d, s.id, done()).await, json!({}));
        }
        d.breaker_spend(None).await;
        assert!(d.breakers().is_empty());
        assert!(d.breaker_needs().is_empty());
        assert_eq!(d.budgets_view().await.unwrap().rules, Rules::default());
        d.prompt(s.id, "and again", Some(agent())).unwrap();
        assert!(d.breaker_refuses_start(Some(&agent())).is_ok());
    })
    .await;
}

#[tokio::test]
async fn a_loop_is_steered_in_its_turn_and_held_from_the_next_until_the_person_speaks() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.set_breaker_rules(Rules { loops: true }).await.unwrap();
        let s = new_session(&d, dir.path()).await;
        post(&d, s.id, prompt()).await;
        for _ in 0..6 {
            assert_eq!(post(&d, s.id, tool("cargo test -p atlas")).await, json!({}));
        }
        // The steer goes with the next tool result, once.
        let said = post(&d, s.id, done()).await;
        let context = said["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        assert!(context.contains("cargo test -p atlas") && context.contains("6 times"), "{context}");
        assert_eq!(said["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert_eq!(post(&d, s.id, done()).await, json!({}));
        // It loops on: held, but this turn finishes.
        for _ in 0..6 {
            assert!(!denied(&post(&d, s.id, tool("cargo test -p atlas")).await));
        }
        let needs = d.breaker_needs();
        assert_eq!(needs.len(), 1);
        assert_eq!(
            needs[0].answer,
            Answer::Decide {
                at: DecideRef::Breaker {
                    subject: super::session_subject(s.id)
                }
            }
        );
        assert!(needs[0].ask.contains("after its turn"), "{}", needs[0].ask);
        assert!(matches!(
            d.prompt(s.id, "keep going", Some(agent())),
            Err(DeskError::Conflict(_))
        ));
        post(&d, s.id, stop()).await;
        // Its own waking is a new turn: held.
        post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "<task-notification>x</task-notification>"})).await;
        let refused = post(&d, s.id, tool("ls")).await;
        assert!(denied(&refused), "{refused}");
        assert!(refused.pointer("/hookSpecificOutput/permissionDecisionReason").unwrap().as_str().unwrap().contains("Continue"));
        // The person typing to it lets it go on.
        post(&d, s.id, prompt()).await;
        assert_eq!(post(&d, s.id, tool("ls")).await, json!({}));
        assert!(d.breaker_needs().is_empty());
    })
    .await;
}

#[tokio::test]
async fn a_session_taken_back_mid_turn_while_held_finishes_that_turn() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.set_breaker_rules(Rules { loops: true }).await.unwrap();
        let s = new_session(&d, dir.path()).await;
        post(&d, s.id, prompt()).await;
        for _ in 0..12 {
            post(&d, s.id, tool("cargo test")).await;
        }
        assert_eq!(d.breaker_needs().len(), 1);
        // As a hub that restarted knows it: held, with nothing of this run.
        if let Some(l) = d.0.state.lock().live.get_mut(&s.id) {
            l.breaker = Default::default();
        }
        assert!(!denied(&post(&d, s.id, tool("ls")).await), "the turn it is on finishes");
        post(&d, s.id, stop()).await;
        post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "<task-notification>x</task-notification>"})).await;
        assert!(denied(&post(&d, s.id, tool("ls")).await), "the next is held");
    })
    .await;
}

#[tokio::test]
async fn a_session_over_its_budget_finishes_its_turn_and_waits_for_continue() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        d.put_budget(budget(BudgetScope::Session(s.id), 1.0), By::you())
            .await
            .unwrap();
        post(&d, s.id, prompt()).await;
        spent(&d, s.id, 0.85).await;
        d.breaker_spend(Some(vec![s.id])).await;
        let warned = post(&d, s.id, done()).await;
        assert!(warned["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("85%"));
        assert!(d.breaker_needs().is_empty(), "a warning asks nothing");
        spent(&d, s.id, 0.30).await;
        d.breaker_spend(Some(vec![s.id])).await;
        let needs = d.breaker_needs();
        assert_eq!(needs.len(), 1);
        assert!(
            needs[0]
                .ask
                .contains("115% of its day budget ($1.15 of $1)"),
            "{}",
            needs[0].ask
        );
        assert!(needs[0].why.as_deref().unwrap().contains("Keep paused"));
        assert!(
            !denied(&post(&d, s.id, tool("ls")).await),
            "its turn goes on"
        );
        post(&d, s.id, stop()).await;
        // Keep paused: the need goes, the hold stays.
        d.breaker_keep(&super::session_subject(s.id)).unwrap();
        assert!(d.breaker_needs().is_empty());
        assert!(d.prompt(s.id, "next", Some(agent())).is_err());
        let view = d.budgets_view().await.unwrap();
        assert_eq!(view.breakers.len(), 1);
        assert!(view.budgets[0].standing.as_ref().unwrap().used > 1.1);
        d.breaker_continue(&super::session_subject(s.id), By::you())
            .unwrap();
        d.prompt(s.id, "next", Some(agent())).unwrap();
        // Released for the rest of the day.
        spent(&d, s.id, 1.0).await;
        d.breaker_spend(Some(vec![s.id])).await;
        assert!(d.breaker_needs().is_empty());
    })
    .await;
}

#[tokio::test]
async fn a_desk_total_holds_agents_new_starts_never_the_persons_and_never_running_ones() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let s = new_session(&d, dir.path()).await;
        d.put_budget(budget(BudgetScope::Desk, 11.0), By::you())
            .await
            .unwrap();
        spent(&d, s.id, 9.13).await;
        d.breaker_spend(Some(vec![s.id])).await;
        let needs = d.breaker_needs();
        assert_eq!(needs.len(), 1);
        assert_eq!(
            needs[0].ask,
            "Desk is at 83% of its day budget ($9.13 of $11). Agents can't start new sessions for it."
        );
        let start = || NewSession {
            engine: Engine::Claude,
            view: None,
            name: "more".into(),
            cwd: dir.path().to_string_lossy().into_owned(),
            goal: String::new(),
            worktree: None,
            prompt: None,
            approvals: None,
            approve_for_me: false,
            account: None,
            env: Default::default(),
        };
        assert!(matches!(d.create(start(), Some(agent())).await, Err(DeskError::Conflict(_))));
        d.create(start(), Some(By::you())).await.unwrap();
        // The running session carries on under its own limits.
        assert_eq!(post(&d, s.id, tool("ls")).await, json!({}));
        d.breaker_continue(&super::pool_subject(&workforce_core::budget::Pool::Desk), By::you())
            .unwrap();
        d.create(start(), Some(agent())).await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn a_budget_for_a_group_that_isnt_there_is_refused() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let mut b = budget(BudgetScope::Desk, 5.0);
        b.group = Some("nowhere".into());
        assert!(matches!(
            d.put_budget(b, By::you()).await,
            Err(DeskError::Invalid(_))
        ));
        let mut b = budget(BudgetScope::Desk, 5.0);
        b.on_exceed = Some(OnExceed::Warn);
        let kept = d.put_budget(b, By::you()).await.unwrap();
        assert_eq!(d.budget_list(), std::slice::from_ref(&kept));
        d.delete_budget(kept.id, By::you()).await.unwrap();
        assert!(d.budget_list().is_empty());
    })
    .await;
}

/// Two agents' use, every billing.
struct Agents;

impl super::AgentSpend for Agents {
    fn spent(
        &self,
        _: chrono::DateTime<Utc>,
        _: chrono::DateTime<Utc>,
    ) -> eyre::Result<Vec<(String, crate::budget_store::Used)>> {
        Ok(vec![(
            "atlas".into(),
            crate::budget_store::Used {
                tokens: 10_000,
                usd: 9.0,
            },
        )])
    }
}

#[tokio::test]
async fn an_agent_over_its_budget_has_its_next_turns_held_until_continue() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        assert!(d.agent_budgets("atlas").is_empty());
        let each = d
            .put_budget(budget(BudgetScope::EachAgent, 20.0), By::you())
            .await
            .unwrap();
        let own = d
            .put_budget(budget(BudgetScope::Agent("atlas".into()), 8.0), By::you())
            .await
            .unwrap();
        assert_eq!(d.agent_budgets("atlas"), std::slice::from_ref(&own));
        assert_eq!(d.agent_budgets("scout"), [each]);
        let now = Utc::now();
        let over = |used| {
            vec![workforce_core::budget::standing_of(
                &own,
                used,
                now - chrono::Duration::hours(1),
                now + chrono::Duration::hours(1),
            )]
        };
        assert!(matches!(d.agent_stand("atlas", "Atlas", over(6.6)), super::AgentVerdict::Warn(w) if w == "Atlas has used 82% of its day budget ($6.60 of $8)."));
        assert_eq!(d.agent_stand("atlas", "Atlas", over(7.0)), super::AgentVerdict::Proceed, "told once");
        let super::AgentVerdict::Held(why) = d.agent_stand("atlas", "Atlas", over(8.4)) else {
            panic!("over its budget")
        };
        assert!(why.starts_with("Atlas is at 105% of its day budget ($8.40 of $8)."), "{why}");
        let needs = d.breaker_needs();
        assert_eq!((needs[0].who.name.as_str(), needs[0].id.as_str()), ("Atlas", "breaker:agent:atlas"));
        assert!(d.agent_held("atlas").is_some());
        d.breaker_continue(&super::agent_subject("atlas"), By::you()).unwrap();
        assert!(d.agent_held("atlas").is_none());
        assert_eq!(d.agent_stand("atlas", "Atlas", over(9.0)), super::AgentVerdict::Proceed);
    })
    .await;
}

#[tokio::test]
async fn agents_use_counts_toward_desks_total() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        d.spend_agents_with(std::sync::Arc::new(Agents));
        d.put_budget(budget(BudgetScope::Desk, 10.0), By::you())
            .await
            .unwrap();
        let view = d.budgets_view().await.unwrap();
        assert_eq!(view.budgets[0].standing.as_ref().unwrap().used, 9.0);
        d.breaker_spend(None).await;
        assert_eq!(d.breaker_needs().len(), 1, "at 90%, new starts are held");
    })
    .await;
}
