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
        preference: None,
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
        post(&d, s.id, json!({"hook_event_name": "UserPromptSubmit", "prompt": "try another way"})).await;
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
            fresh_worktree: false,
            engine: Engine::Claude,
            view: None,
            name: "more".into(),
            cwd: dir.path().to_string_lossy().into_owned(),
            goal: String::new(),
            worktree: None,
            prompt: None,
            approvals: None,
            approve_for_me: false,
            fast: false,
            model: None,
            account: None,
            env: Default::default(),
            group: None,
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

/// A session held by a loop, its turn over: what follows is a new turn.
async fn held_session(d: &Desk, dir: &std::path::Path) -> Uuid {
    d.set_breaker_rules(Rules { loops: true }).await.unwrap();
    let s = new_session(d, dir).await;
    post(d, s.id, prompt()).await;
    for _ in 0..12 {
        post(d, s.id, tool("cargo test")).await;
    }
    post(d, s.id, stop()).await;
    assert_eq!(d.breaker_needs().len(), 1);
    s.id
}

#[tokio::test]
async fn only_words_the_person_typed_let_a_held_session_go_on() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let id = held_session(&d, dir.path()).await;
        // No one named is not the person.
        assert!(matches!(
            d.prompt(id, "go on", None),
            Err(DeskError::Conflict(_))
        ));
        // What Desk typed for someone else, heard as the session's prompt.
        d.breaker_typed(id, "keep going", Some(&agent()));
        post(
            &d,
            id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "keep going"}),
        )
        .await;
        assert!(
            denied(&post(&d, id, tool("ls")).await),
            "an agent's words hold on"
        );
        // The same words again (a loop, a schedule).
        post(
            &d,
            id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "keep going"}),
        )
        .await;
        assert!(denied(&post(&d, id, tool("ls")).await), "a repeat holds on");
        assert_eq!(d.breaker_needs().len(), 1);
        // The person's own words.
        post(
            &d,
            id,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": "fine, fix the test"}),
        )
        .await;
        assert_eq!(post(&d, id, tool("ls")).await, json!({}));
        assert!(d.breaker_needs().is_empty());
        let mut continued = Vec::new();
        for _ in 0..100 {
            continued = d
                .activity(crate::crew::Filter::All, None, 50)
                .await
                .unwrap()
                .events
                .into_iter()
                .filter(|e| e.kind == crate::crew::Kind::Continued)
                .collect();
            if !continued.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(continued.len(), 1);
        assert_eq!(continued[0].actor.kind, crate::crew::PartyKind::You);
    })
    .await;
}

#[tokio::test]
async fn no_one_but_the_person_resumes_or_restarts_a_held_session() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let id = held_session(&d, dir.path()).await;
        assert!(matches!(
            d.restart(id, Some(crate::engine_accounts::CONTINUE.into()), agent())
                .await,
            Err(DeskError::Conflict(_))
        ));
        let crew = By {
            party: Party::manager("desk", "Desk"),
            reason: None,
            suggestion_id: None,
            hop: 0,
            preference: None,
        };
        assert!(matches!(
            d.restart(id, None, crew.clone()).await,
            Err(DeskError::Conflict(_))
        ));
        assert!(matches!(
            d.resume(id, Some("go".into()), Some(crew)).await,
            Err(DeskError::Conflict(_))
        ));
        assert_eq!(d.breaker_needs().len(), 1, "still held");
    })
    .await;
}

/// A boss running a session the breaker holds answers nothing for it:
/// only the person lets it go on.
#[tokio::test]
async fn a_boss_never_answers_for_a_session_the_breaker_holds() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let id = held_session(&d, dir.path()).await;
        let sarah = Party::manager("sarah", "Sarah");
        d.take_on(
            id,
            &sarah,
            crate::charge::How::Handed,
            &Default::default(),
            &By::you(),
        )
        .await
        .unwrap();
        d.boss_bounds_with(crate::charge::Bounds {
            home: dir.path().to_path_buf(),
            ..Default::default()
        });
        let asking = {
            let d = d.clone();
            tokio::spawn(async move {
                post(
                    &d,
                    id,
                    json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash",
                        "tool_input": {"command": "cargo test"}}),
                )
                .await
            })
        };
        let mut approval = None;
        for _ in 0..200 {
            approval = d.view(id).unwrap().pending.map(|p| p.id);
            if approval.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let approval = approval.expect("it asks");
        let boss = By::manager("sarah", "Sarah", "Tests are fine.");
        for decision in [crate::model::Decision::Allow, crate::model::Decision::Deny] {
            match d.decide(id, approval, decision, Some(boss.clone())) {
                Err(DeskError::Conflict(why)) => assert!(why.contains("breaker"), "{why}"),
                other => panic!("a boss answered for a held session: {other:?}"),
            }
        }
        assert_eq!(d.breaker_needs().len(), 1, "still held");
        d.decide(id, approval, crate::model::Decision::Deny, Some(By::you()))
            .unwrap();
        asking.await.unwrap();
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
        // A scheduled run asking first says no warning, and leaves it for the turn.
        assert_eq!(d.agent_stand("atlas", "Atlas", over(6.6), true), super::AgentVerdict::Proceed);
        assert!(matches!(d.agent_stand("atlas", "Atlas", over(6.6), false), super::AgentVerdict::Warn(w) if w == "Atlas has used 82% of its day budget ($6.60 of $8)."));
        assert_eq!(d.agent_stand("atlas", "Atlas", over(7.0), false), super::AgentVerdict::Proceed, "told once");
        let super::AgentVerdict::Held(why) = d.agent_stand("atlas", "Atlas", over(8.4), false) else {
            panic!("over its budget")
        };
        assert!(why.starts_with("Atlas is at 105% of its day budget ($8.40 of $8)."), "{why}");
        let needs = d.breaker_needs();
        assert_eq!((needs[0].who.name.as_str(), needs[0].id.as_str()), ("Atlas", "breaker:agent:atlas"));
        assert!(d.agent_held("atlas").is_some());
        d.breaker_continue(&super::agent_subject("atlas"), By::you()).unwrap();
        assert!(d.agent_held("atlas").is_none());
        assert_eq!(d.agent_stand("atlas", "Atlas", over(9.0), false), super::AgentVerdict::Proceed);
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

#[tokio::test]
async fn an_agents_hold_lapses_when_its_period_ends() {
    with_test_pool(build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let d = desk(pool, dir.path()).await;
        let own = d
            .put_budget(budget(BudgetScope::Agent("atlas".into()), 8.0), By::you())
            .await
            .unwrap();
        let ended = Utc::now() - chrono::Duration::minutes(1);
        let at = |used, from: chrono::DateTime<Utc>, to| {
            vec![workforce_core::budget::standing_of(&own, used, from, to)]
        };
        // Held over a period that has since ended.
        assert!(matches!(
            d.agent_stand(
                "atlas",
                "Atlas",
                at(9.0, ended - chrono::Duration::days(1), ended),
                true
            ),
            super::AgentVerdict::Held(_)
        ));
        let today = (ended, ended + chrono::Duration::days(1));
        assert_eq!(
            d.agent_stand("atlas", "Atlas", at(0.5, today.0, today.1), true),
            super::AgentVerdict::Proceed,
            "a new day, under its budget"
        );
        assert!(d.agent_held("atlas").is_none());
    })
    .await;
}
