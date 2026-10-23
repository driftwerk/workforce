//! Decision steps end to end on one hub: cards, settlement races, authority,
//! following a decision, delivery and the views.

use std::collections::HashMap;
use std::sync::Arc;

use workforce_common::channels::ChannelSurface;
use workforce_core::access::Visibility;
use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};
use workforce_orchestrator::workflow::decision::{
    self as run_decision, CardTarget, Closed, DecidedBy, RunDecision,
};
use workforce_orchestrator::workflow::instance::{Actor, InstanceStatus, WorkflowInstance};

use super::testing::{decision_steps, hub, member, Hub, RecordingSurface};
use super::{
    decision::{DecisionAsk, DecisionRefused, ResumeAsk, ResumeRefused, Resumed},
    OwnedDefinition, Steer, WorkflowRuntime,
};
use crate::run_steering::{SteerAction, SteerRefused, Steerer};

const RUN_CHANNEL: &str = "CRUN000001";

/// A hub whose Slack is a recording surface, and the digest's workflow.
struct Desk {
    hub: Hub,
    surface: Arc<RecordingSurface>,
    key: String,
    dan: String,
}

impl Desk {
    async fn new(deciders: impl FnOnce(&Hub) -> String, extra: &str) -> Self {
        Self::with(Visibility::Private, false, deciders, extra).await
    }

    async fn with(
        visibility: Visibility,
        by_others: bool,
        deciders: impl FnOnce(&Hub) -> String,
        extra: &str,
    ) -> Self {
        let hub = hub().await;
        let surface = hub.with_slack();
        let dan = person(&hub, "Dan", None, None);
        let key = hub
            .define_steps(
                visibility,
                by_others,
                &decision_steps(&deciders(&hub), extra),
            )
            .await;
        Self {
            hub,
            surface,
            key,
            dan,
        }
    }

    fn runtime(&self) -> &Arc<WorkflowRuntime> {
        &self.hub.runtime
    }

    /// A run Alice started, delivering to `channel`, gone on to its decision.
    async fn waiting_in(&self, channel: Option<&str>) -> String {
        self.hub
            .run_to_decision(&self.key, Some(&self.hub.alice), channel)
            .await
    }

    async fn waiting(&self) -> String {
        self.waiting_in(Some(RUN_CHANNEL)).await
    }

    fn instance(&self, run: &str) -> WorkflowInstance {
        self.runtime().load_instance(run).unwrap()
    }

    fn decision(&self, run: &str) -> RunDecision {
        RunDecision::read(&self.instance(run).variables).expect("the run holds a decision")
    }

    async fn press(
        &self,
        run: &str,
        who: &Steerer,
        choice: &str,
    ) -> Result<super::decision::DecisionMade, DecisionRefused> {
        self.runtime()
            .decide(
                run,
                DecisionAsk {
                    gate: Some(self.decision(run).gate),
                    choice: choice.into(),
                    feedback: None,
                },
                who,
            )
            .await
    }

    fn bob(&self) -> Steerer {
        member(&self.hub.bob)
    }

    /// Every card update's status line.
    async fn closed_cards(&self) -> Vec<String> {
        self.surface
            .updated
            .lock()
            .await
            .iter()
            .map(|(_, _, m)| serde_json::to_string(&m.blocks).unwrap())
            .collect()
    }
}

/// A member called `name`, with the email and Slack id given.
fn person(hub: &Hub, name: &str, email: Option<&str>, slack: Option<&str>) -> String {
    let mut identity = Identity::new(name, IdentityRole::Member);
    identity.email = email.map(str::to_string);
    if let Some(slack) = slack {
        identity.linked_ids.insert("slack".into(), slack.into());
    }
    store(hub).create(&identity).unwrap();
    identity.id.to_string()
}

fn store(hub: &Hub) -> Arc<dyn workforce_identity::store::IdentityStore> {
    hub.service.identity_store.read().clone().unwrap()
}

/// Give the identity `id` an email and a Slack id.
fn link(hub: &Hub, id: &str, email: Option<&str>, slack: Option<&str>) {
    let store = store(hub);
    let mut identity = store
        .get_by_id(uuid::Uuid::parse_str(id).unwrap())
        .unwrap()
        .unwrap();
    identity.email = email.map(str::to_string);
    if let Some(slack) = slack {
        identity.linked_ids.insert("slack".into(), slack.into());
    }
    store.update(&identity).unwrap();
}

/// Whether `holds` comes true within five seconds: for what a run's loop
/// does after its status is already final.
async fn eventually(holds: impl Fn() -> bool) -> bool {
    for _ in 0..500 {
        if holds() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    holds()
}

fn card_count(blocks: &[String], needle: &str) -> usize {
    blocks.iter().filter(|b| b.contains(needle)).count()
}

#[tokio::test]
async fn a_decision_step_posts_one_card_and_waits_at_an_open_gate() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;

    let instance = desk.instance(&run);
    let decision = run_decision::open_in(&instance).expect("an open decision");
    assert_eq!(instance.status, InstanceStatus::Paused);
    assert!(decision.posted);
    assert_eq!(decision.cards.len(), 1);
    assert_eq!(decision.title, "Review the digest");
    assert_eq!(decision.body.as_deref(), Some("Today's digest."));
    let cards: Vec<_> = desk
        .surface
        .posted_to(RUN_CHANNEL)
        .await
        .into_iter()
        .filter(|m| m.blocks.is_some())
        .collect();
    assert_eq!(cards.len(), 1);
    let blocks = serde_json::to_string(&cards[0].blocks).unwrap();
    assert!(blocks.contains("wf_decision_0") && blocks.contains("Waiting on Bob"));
    assert!(!desk
        .surface
        .sent
        .lock()
        .await
        .iter()
        .any(|m| m.text.contains("Approval required")));
}

#[tokio::test]
async fn two_deciders_pressing_at_once_settle_it_once() {
    let desk = Desk::new(|hub| format!("{}, {}", hub.bob, hub.carol), "").await;
    let run = desk.waiting().await;
    let (bob, carol) = (desk.bob(), desk.hub.admin());

    let (sent, cancelled) = tokio::join!(
        desk.press(&run, &bob, "send"),
        desk.press(&run, &carol, "cancel")
    );

    let won: Vec<bool> = vec![sent.is_ok(), cancelled.is_ok()];
    assert_eq!(
        won.iter().filter(|w| **w).count(),
        1,
        "{sent:?} {cancelled:?}"
    );
    let lost = sent.err().or(cancelled.err()).unwrap();
    assert!(matches!(lost, DecisionRefused::Decided { .. }), "{lost}");
    let settled = desk.hub.settled(&run).await;
    assert!(settled.is_terminal());
}

#[tokio::test]
async fn a_double_click_by_one_decider_settles_once() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    let bob = desk.bob();

    let (first, second) = tokio::join!(
        desk.press(&run, &bob, "send"),
        desk.press(&run, &bob, "send")
    );

    assert_eq!(
        [first.is_ok(), second.is_ok()]
            .iter()
            .filter(|w| **w)
            .count(),
        1
    );
    let lost = first.err().or(second.err()).unwrap();
    assert_eq!(
        lost,
        DecisionRefused::Decided {
            summary: "Sent by Bob".into()
        }
    );
    assert_eq!(desk.hub.ran(&run).await, ["draft", "review", "send"]);
}

#[tokio::test]
async fn a_late_press_on_the_old_card_after_a_loop_back_is_outdated() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    let first_gate = desk.decision(&run).gate;
    desk.press(&run, &desk.bob(), "redraft").await.unwrap();
    desk.hub.settled(&run).await;
    assert_ne!(desk.decision(&run).gate, first_gate, "a fresh gate");

    let late = desk
        .runtime()
        .decide(
            &run,
            DecisionAsk {
                gate: Some(first_gate),
                choice: "send".into(),
                feedback: None,
            },
            &desk.bob(),
        )
        .await;

    assert_eq!(late.unwrap_err(), DecisionRefused::Outdated);
    assert!(run_decision::open_in(&desk.instance(&run)).is_some());
}

#[tokio::test]
async fn a_press_after_the_run_was_stopped_is_refused_and_every_card_says_stopped() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    let gate = desk.decision(&run).gate;
    let alice = member(&desk.hub.alice);
    let by = desk
        .runtime()
        .authorize_steer(&run, &alice, SteerAction::Abort)
        .await
        .unwrap();
    desk.runtime().abort_workflow(&run, &by).await.unwrap();

    let late = desk
        .runtime()
        .decide(
            &run,
            DecisionAsk {
                gate: Some(gate),
                choice: "send".into(),
                feedback: None,
            },
            &desk.bob(),
        )
        .await;

    assert_eq!(
        late.unwrap_err(),
        DecisionRefused::Ended {
            status: "cancelled".into()
        }
    );
    let closed = desk.closed_cards().await;
    assert_eq!(closed.len(), 1);
    assert!(closed[0].contains("Stopped by Alice"), "{closed:?}");
    assert!(desk.decision(&run).cards_closed);
}

#[tokio::test]
async fn a_typed_reply_and_a_press_race_to_one_winner() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    let thread = desk.instance(&run).variables["_thread_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (typed_run, gate, outcome) = desk
        .runtime()
        .decision_reply(RUN_CHANNEL, &thread, "Cancel")
        .expect("the reply names a choice");
    assert_eq!(
        (typed_run.as_str(), outcome.as_str()),
        (run.as_str(), "cancel")
    );

    let bob = desk.bob();
    let typed = desk.runtime().decide(
        &run,
        DecisionAsk {
            gate: Some(gate),
            choice: outcome,
            feedback: None,
        },
        &bob,
    );
    let (typed, pressed) = tokio::join!(typed, desk.press(&run, &bob, "send"));

    assert_eq!(
        [typed.is_ok(), pressed.is_ok()]
            .iter()
            .filter(|w| **w)
            .count(),
        1
    );
}

/// Move the run's decision deadline into the past.
fn expire(desk: &Desk, run: &str) {
    desk.runtime()
        .update_instance(run, |i| {
            let decision = RunDecision::read(&i.variables).unwrap();
            RunDecision {
                expires_at: Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
                ..decision
            }
            .write(&mut i.variables);
            Ok::<_, eyre::Report>(())
        })
        .unwrap();
}

#[tokio::test]
async fn an_expiry_racing_a_press_has_one_winner() {
    let desk = Desk::new(
        |hub| hub.bob.clone(),
        "expires_after = \"1h\"\non_expiry = \"cancel\"",
    )
    .await;
    let run = desk.waiting().await;
    expire(&desk, &run);

    let bob = desk.bob();
    let ((), pressed) = tokio::join!(
        desk.runtime().decision_sweep(),
        desk.press(&run, &bob, "send")
    );

    let settled = desk.hub.settled(&run).await;
    match pressed {
        Ok(_) => assert_eq!(settled.status, InstanceStatus::Completed),
        Err(refused) => {
            assert_eq!(
                refused,
                DecisionRefused::Decided {
                    summary: "Cancel (expired)".into()
                }
            );
            assert_eq!(settled.status, InstanceStatus::Cancelled);
        }
    }
}

#[tokio::test]
async fn an_expired_cancel_frees_the_slot_and_the_next_scheduled_run_starts() {
    let desk = Desk::new(
        |hub| hub.bob.clone(),
        "expires_after = \"1h\"\non_expiry = \"cancel\"",
    )
    .await;
    let run = desk.waiting().await;
    let next = desk.hub.run_of(&desk.key, Some(&desk.hub.alice)).await;
    let mut queued = desk.instance(&next);
    queued.status = InstanceStatus::Queued;
    desk.runtime().save_instance(&queued).unwrap();
    expire(&desk, &run);

    desk.runtime().decision_sweep().await;

    let ended = desk.hub.settled(&run).await;
    assert_eq!(ended.status, InstanceStatus::Cancelled);
    assert_eq!(
        ended.variables["_cancellation_reason"],
        "Expired at 'review': Cancel"
    );
    assert!(eventually(|| desk.instance(&next).status != InstanceStatus::Queued).await);
    let closed = desk.closed_cards().await;
    assert!(closed.iter().any(|c| c.contains("Expired")), "{closed:?}");
}

#[tokio::test]
async fn only_named_deciders_decide_and_admins_and_the_starter_do_not() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;

    for who in [member(&desk.hub.alice), desk.hub.admin()] {
        assert_eq!(
            desk.press(&run, &who, "send").await.unwrap_err(),
            DecisionRefused::Steer(SteerRefused::NotDecider {
                deciders: "Bob".into()
            })
        );
    }
    let made = desk.press(&run, &desk.bob(), "send").await.unwrap();
    assert_eq!(
        (made.label.as_str(), made.summary.as_str()),
        ("Send", "Sent by Bob")
    );
}

#[tokio::test]
async fn only_the_starter_or_an_admin_may_stop_a_run_at_a_decision() {
    let desk = Desk::with(Visibility::Global, true, |hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;

    let refused = desk
        .runtime()
        .authorize_steer(&run, &member(&desk.dan), SteerAction::Abort)
        .await;
    assert_eq!(refused.unwrap_err(), SteerRefused::StopAtDecision);
    for who in [member(&desk.hub.alice), desk.hub.admin()] {
        assert!(desk
            .runtime()
            .authorize_steer(&run, &who, SteerAction::Abort)
            .await
            .is_ok());
    }
}

#[tokio::test]
async fn a_starter_who_is_a_decider_is_refused_when_self_decision_is_off() {
    let desk = Desk::new(
        |hub| format!("{}, {}", hub.alice, hub.bob),
        "allow_self_decision = false",
    )
    .await;
    let run = desk.waiting().await;

    assert_eq!(
        desk.press(&run, &member(&desk.hub.alice), "send")
            .await
            .unwrap_err(),
        DecisionRefused::Steer(SteerRefused::OwnDecision {
            deciders: "Bob".into()
        })
    );
    assert!(desk.press(&run, &desk.bob(), "send").await.is_ok());
}

#[tokio::test]
async fn an_agent_with_nobody_behind_it_cannot_decide() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    let agent = Steerer {
        writer: crate::def_write_auth::DefWriter {
            identity_id: None,
            is_admin: false,
        },
        actor: Actor::Agent {
            id: "example".into(),
        },
    };

    assert!(matches!(
        desk.press(&run, &agent, "send").await,
        Err(DecisionRefused::Steer(SteerRefused::NotDecider { .. }))
    ));
}

#[tokio::test]
async fn a_workflow_step_acting_as_a_decider_cannot_decide() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;

    let pressed = workforce_agent::runner::CURRENT_SESSION_KEY
        .scope(
            "workflow:another-run:step".to_string(),
            desk.press(&run, &desk.bob(), "send"),
        )
        .await;

    assert!(matches!(
        pressed,
        Err(DecisionRefused::Steer(SteerRefused::NotDecider { .. }))
    ));
    assert!(run_decision::open_in(&desk.instance(&run)).is_some());
    desk.press(&run, &desk.bob(), "send").await.unwrap();
}

#[tokio::test]
async fn a_press_settled_just_before_a_stop_still_closes_its_cards() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    settle_only(&desk, &run, "send");
    let alice = member(&desk.hub.alice);
    let by = desk
        .runtime()
        .authorize_steer(&run, &alice, SteerAction::Abort)
        .await
        .unwrap();

    desk.runtime().abort_workflow(&run, &by).await.unwrap();

    assert_eq!(desk.instance(&run).status, InstanceStatus::Cancelled);
    let closed = desk.closed_cards().await;
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert!(closed[0].contains("by Bob"), "{closed:?}");
    assert!(desk.decision(&run).cards_closed);
}

#[tokio::test]
async fn resume_reject_goto_skip_retry_and_pause_are_refused_while_a_decision_is_open() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    let alice = member(&desk.hub.alice);

    for action in [
        SteerAction::Resume,
        SteerAction::Reject,
        SteerAction::Goto,
        SteerAction::Skip,
        SteerAction::Retry,
        SteerAction::Pause,
    ] {
        assert!(
            matches!(
                desk.runtime().authorize_steer(&run, &alice, action).await,
                Err(SteerRefused::AwaitingDecision { .. })
            ),
            "{action:?}"
        );
    }
    let hub = Steer::hub();
    let resumed = desk
        .runtime()
        .resume_workflow(&run, Some("send".into()), None, None, None, &hub)
        .await
        .unwrap_err();
    assert!(
        resumed.to_string().contains("waiting on a decision"),
        "{resumed}"
    );
    let skipped = desk.runtime().skip_step(&run, &hub).await.unwrap_err();
    assert!(
        skipped.to_string().contains("waiting on a decision"),
        "{skipped}"
    );
    assert!(desk.runtime().goto_step(&run, "send", &hub).await.is_err());
    assert!(run_decision::open_in(&desk.instance(&run)).is_some());
}

#[tokio::test]
async fn a_maintenance_resume_never_passes_an_open_decision() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    desk.runtime()
        .set_instance_variable(&run, super::SUSPENSION_HELD_AT, "2026-10-16T09:00:00Z")
        .unwrap();

    desk.runtime().resume_suspension_held().await;

    let instance = desk.instance(&run);
    assert_eq!(instance.status, InstanceStatus::Paused);
    assert!(run_decision::open_in(&instance).is_some());
}

#[tokio::test]
async fn a_decision_survives_a_restart_and_a_press_after_it_settles_it() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    let restarted = Arc::new(WorkflowRuntime::new(
        Arc::clone(&desk.hub.service),
        Arc::clone(&desk.hub.service.telemetry_store),
        std::env::temp_dir(),
    ));
    let owned: HashMap<String, OwnedDefinition> = desk.runtime().definitions.read().await.clone();
    *restarted.definitions.write().await = owned;

    restarted
        .decide(
            &run,
            DecisionAsk {
                gate: None,
                choice: "Send".into(),
                feedback: None,
            },
            &desk.bob(),
        )
        .await
        .unwrap();

    assert_eq!(desk.hub.ran(&run).await, ["draft", "review", "send"]);
}

/// Settle the run's decision as a press by Bob would, without starting the
/// loop that carries it out: a hub that died right after the write.
fn settle_only(desk: &Desk, run: &str, outcome: &str) {
    desk.runtime()
        .update_instance(run, |i| {
            let decision = run_decision::open_in(i).unwrap();
            let choice = decision.choice(outcome).unwrap().clone();
            run_decision::settle(
                i,
                &choice,
                DecidedBy::Person {
                    id: desk.hub.bob.clone(),
                    display: "Bob".into(),
                    slack_user_id: None,
                },
                None,
                chrono::Utc::now(),
            )
            .map_err(|e| eyre::eyre!(e))
        })
        .unwrap();
}

#[tokio::test]
async fn a_run_settled_but_not_followed_is_carried_on_by_recovery() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    settle_only(&desk, &run, "send");
    assert_eq!(desk.instance(&run).status, InstanceStatus::Running);

    desk.runtime().recover_running_instances().await;

    assert_eq!(desk.hub.ran(&run).await, ["draft", "review", "send"]);
    assert!(run_decision::awaiting_follow(&desk.instance(&run)).is_none());
}

#[tokio::test]
async fn a_goto_button_keeps_its_step_after_the_definition_drops_the_branch() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    let without_branch = decision_steps(&desk.hub.bob, "")
        .replace("branches = [{ when = \"send\", goto = \"send\" }]\n", "")
        .replace("done = \"Sent\"\n", "done = \"Sent\"\nend = \"complete\"\n");
    desk.hub
        .define_steps(Visibility::Private, false, &without_branch)
        .await;

    desk.press(&run, &desk.bob(), "send").await.unwrap();

    assert_eq!(desk.hub.ran(&run).await, ["draft", "review", "send"]);
}

#[tokio::test]
async fn a_button_whose_step_was_removed_is_refused_and_the_run_keeps_waiting() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    let without_send = decision_steps(&desk.hub.bob, "")
        .replace("branches = [{ when = \"send\", goto = \"send\" }]\n", "")
        .replace("done = \"Sent\"\n", "done = \"Sent\"\nend = \"complete\"\n")
        .replace("goto = \"send\"\nset", "end = \"complete\"\nset");
    let without_send =
        without_send[..without_send.find("[[step]]\nid = \"send\"").unwrap()].to_string();
    desk.hub
        .define_steps(Visibility::Private, false, &without_send)
        .await;

    let refused = desk.press(&run, &desk.bob(), "send").await.unwrap_err();

    assert_eq!(
        refused,
        DecisionRefused::StepGone {
            step: "send".into()
        }
    );
    assert!(run_decision::open_in(&desk.instance(&run)).is_some());
    assert_eq!(desk.instance(&run).status, InstanceStatus::Paused);
}

#[tokio::test]
async fn an_end_complete_button_completes_without_running_another_step() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;

    desk.press(&run, &desk.bob(), "done").await.unwrap();

    let ended = desk.hub.settled(&run).await;
    assert_eq!(ended.status, InstanceStatus::Completed);
    assert_eq!(desk.hub.ran(&run).await, ["draft", "review"]);
}

#[tokio::test]
async fn an_end_cancel_button_cancels_the_run_and_its_children() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    let child = desk.hub.run_of(&desk.key, Some(&desk.hub.alice)).await;
    let mut started = desk.instance(&child);
    started.parent_instance_id = Some(run.clone());
    desk.runtime().save_instance(&started).unwrap();

    desk.press(&run, &desk.bob(), "cancel").await.unwrap();

    let ended = desk.hub.settled(&run).await;
    assert_eq!(ended.status, InstanceStatus::Cancelled);
    assert_eq!(
        ended.variables["_cancellation_reason"],
        "Ended at 'review': Cancel by Bob"
    );
    assert!(eventually(|| desk.instance(&child).status == InstanceStatus::Cancelled).await);
}

/// Alice's escalation workflow, which renders a report and ends.
async fn define_escalation(desk: &Desk) {
    let def = super::WorkflowDefinition::from_toml(
        "[workflow]\nid = \"escalation\"\nname = \"Escalation\"\n\n\
         [workflow.context]\nsubject = { required = true }\n\n\
         [[step]]\nid = \"note\"\nname = \"Note\"\ntask = \"\"\n\
         report = \"Escalated {{ subject }}.\"\nreport_render = \"template\"\nexit = true\n",
    )
    .unwrap();
    let owned = OwnedDefinition {
        def,
        owner: Some(desk.hub.alice.clone()),
        visibility: Visibility::Private,
        last_editor: Some(desk.hub.alice.clone()),
        run_as: None,
        content_hash: None,
    };
    desk.runtime()
        .definitions
        .write()
        .await
        .insert(owned.key(), owned);
}

const ESCALATE: &str = "[[step.decision.buttons]]\nlabel = \"Escalate\"\noutcome = \"escalate\"\n\
    workflow = { id = \"escalation\", payload = { subject = \"the digest\" } }\n\
    end = \"complete\"";

fn children_of(desk: &Desk, run: &str) -> Vec<workforce_common::persistence::WorkflowInstanceRow> {
    desk.runtime()
        .list_instances(None, None)
        .unwrap()
        .into_iter()
        .filter(|row| row.parent_instance_id.as_deref() == Some(run))
        .collect()
}

#[tokio::test]
async fn a_workflow_button_starts_its_child_once_as_the_runs_principal_even_across_a_restart() {
    let desk = Desk::new(|hub| hub.bob.clone(), ESCALATE).await;
    define_escalation(&desk).await;
    let run = desk.waiting().await;
    settle_only(&desk, &run, "escalate");
    let child = desk
        .hub
        .run_of(
            &format!("{}/escalation", desk.hub.alice),
            Some(&desk.hub.alice),
        )
        .await;
    let mut earlier = desk.instance(&child);
    earlier.parent_instance_id = Some(run.clone());
    desk.runtime().save_instance(&earlier).unwrap();
    desk.runtime()
        .update_instance(&run, |i| {
            i.variables[super::CHILD_STARTED_KEY] = serde_json::json!({
                "step": "review",
                "since": chrono::Utc::now().to_rfc3339(),
                "instance_id": child,
            });
            Ok::<_, eyre::Report>(())
        })
        .unwrap();

    desk.runtime()
        .execute_loop(&run, Some(RUN_CHANNEL.into()), None, None)
        .await
        .unwrap();

    let ended = desk.hub.settled(&run).await;
    assert_eq!(ended.status, InstanceStatus::Completed);
    assert_eq!(
        children_of(&desk, &run).len(),
        1,
        "attached, not started again"
    );
    assert!(matches!(
        RunDecision::read(&ended.variables).unwrap().closed,
        Some(Closed::Decided { child: Some(ref id), followed: true, .. }) if *id == child
    ));
}

#[tokio::test]
async fn a_press_stopped_before_it_is_carried_out_leaves_the_run_cancelled_and_starts_no_child() {
    let desk = Desk::new(|hub| hub.bob.clone(), ESCALATE).await;
    define_escalation(&desk).await;
    let run = desk.waiting().await;
    settle_only(&desk, &run, "escalate");
    let alice = member(&desk.hub.alice);
    let by = desk
        .runtime()
        .authorize_steer(&run, &alice, SteerAction::Abort)
        .await
        .unwrap();
    desk.runtime().abort_workflow(&run, &by).await.unwrap();

    desk.runtime()
        .execute_loop(&run, Some(RUN_CHANNEL.into()), None, None)
        .await
        .unwrap();

    let ended = desk.instance(&run);
    assert_eq!(ended.status, InstanceStatus::Cancelled);
    assert!(children_of(&desk, &run).is_empty());
    assert!(run_decision::awaiting_follow(&ended).is_none());
}

#[tokio::test]
async fn a_child_that_cannot_start_halts_and_a_retry_starts_it() {
    let desk = Desk::new(|hub| hub.bob.clone(), ESCALATE).await;
    let run = desk.waiting().await;

    desk.press(&run, &desk.bob(), "escalate").await.unwrap();

    let halted = desk.hub.settled(&run).await;
    assert_eq!(halted.status, InstanceStatus::Failed);
    let why = halted.variables["_failure_reason"].as_str().unwrap();
    assert!(
        why.starts_with("Decided Escalate, but workflow 'escalation' could not start"),
        "{why}"
    );

    define_escalation(&desk).await;
    let hub = Steer::hub();
    desk.runtime().retry_step(&run, &hub).unwrap();
    desk.runtime()
        .resume_workflow(&run, None, None, None, None, &hub)
        .await
        .unwrap();

    let ended = desk.hub.settled(&run).await;
    assert_eq!(ended.status, InstanceStatus::Completed);
    let children = children_of(&desk, &run);
    assert_eq!(children.len(), 1);
    let child_vars: serde_json::Value = serde_json::from_str(&children[0].variables).unwrap();
    assert_eq!(child_vars["_run_as"], desk.hub.alice.as_str());
}

#[tokio::test]
async fn a_set_button_writes_its_literals_before_the_next_step() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;

    desk.press(&run, &desk.bob(), "flag").await.unwrap();

    let ended = desk.hub.settled(&run).await;
    assert_eq!(ended.variables["escalated"], "yes");
    assert_eq!(
        ended.step_history.last().unwrap().report.as_deref(),
        Some("Sent, escalated yes.")
    );
}

#[tokio::test]
async fn dm_delivery_posts_a_card_per_reachable_decider_and_rewrites_every_card() {
    let desk = Desk::new(
        |hub| format!("{}, {}", hub.bob, hub.carol),
        "channel = \"dm\"",
    )
    .await;
    link(&desk.hub, &desk.hub.bob, None, Some("U-BOB"));
    link(&desk.hub, &desk.hub.carol, None, Some("U-CAROL"));
    let run = desk.waiting().await;

    let cards = desk.decision(&run).cards;
    let mut channels: Vec<&str> = cards.iter().map(|c| c.channel.as_str()).collect();
    channels.sort();
    assert_eq!(channels, ["D-U-BOB", "D-U-CAROL"]);
    let pointer = desk.surface.posted_to(RUN_CHANNEL).await;
    assert!(pointer
        .iter()
        .any(|m| m.text.contains("sent to each of them directly")));

    desk.press(&run, &desk.bob(), "send").await.unwrap();
    desk.hub.settled(&run).await;

    let updated = desk.surface.updated.lock().await;
    let mut rewritten: Vec<&str> = updated
        .iter()
        .map(|(channel, _, _)| channel.as_str())
        .collect();
    rewritten.sort();
    assert_eq!(rewritten, ["D-U-BOB", "D-U-CAROL"]);
}

#[tokio::test]
async fn a_card_that_cannot_be_posted_fails_the_step_and_a_retry_opens_a_fresh_gate() {
    let desk = Desk::new(|hub| hub.bob.clone(), "channel = \"CFAILING01\"").await;
    desk.surface
        .failing
        .lock()
        .insert("CFAILING01".into(), "not_in_channel".into());
    let run = desk.waiting().await;

    let failed = desk.instance(&run);
    assert_eq!(failed.status, InstanceStatus::Failed);
    assert_eq!(
        failed.variables["_failure_reason"],
        "The Workforce app is not in <#CFAILING01>. Invite it to the channel, then retry the step."
    );
    assert!(RunDecision::read(&failed.variables).is_none());
    assert!(failed.last_record_of("review").is_none());

    desk.surface.failing.lock().clear();
    let hub = Steer::hub();
    desk.runtime().retry_step(&run, &hub).unwrap();
    desk.runtime()
        .resume_workflow(&run, None, None, None, None, &hub)
        .await
        .unwrap();
    let posted = || run_decision::open_in(&desk.instance(&run)).filter(|d| d.posted);

    assert!(
        eventually(|| posted().is_some()).await,
        "a fresh gate, posted"
    );
    assert_eq!(posted().unwrap().cards[0].channel, "CFAILING01");
}

#[tokio::test]
async fn a_run_with_no_channel_delivers_by_direct_message() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    link(&desk.hub, &desk.hub.bob, None, Some("U-BOB"));
    let run = desk.waiting_in(None).await;

    let decision = desk.decision(&run);
    assert_eq!(decision.target, CardTarget::DirectMessages);
    assert_eq!(decision.cards.len(), 1);
    assert_eq!(decision.cards[0].channel, "D-U-BOB");
}

#[tokio::test]
async fn a_card_in_another_channel_leaves_a_pointer_and_a_result_in_the_run_thread() {
    let desk = Desk::new(|hub| hub.bob.clone(), "channel = \"COTHER0001\"").await;
    let run = desk.waiting().await;
    assert_eq!(desk.decision(&run).cards[0].channel, "COTHER0001");
    let pointed = desk.surface.posted_to(RUN_CHANNEL).await;
    assert!(pointed.iter().any(|m| m
        .text
        .contains("is waiting on Bob. The card is in <#COTHER0001>.")));

    desk.press(&run, &desk.bob(), "send").await.unwrap();
    desk.hub.settled(&run).await;

    let told = desk.surface.posted_to(RUN_CHANNEL).await;
    assert!(
        told.iter().any(|m| m.text == "✅ *Sent*, decided by Bob"),
        "{told:?}"
    );
}

#[tokio::test]
async fn a_card_in_the_runs_channel_anchors_a_run_with_no_thread_including_its_channel() {
    let desk = Desk::new(|hub| hub.bob.clone(), "channel = \"CRUN000001\"").await;
    let run = desk.waiting_in(None).await;

    let row = desk.runtime().get_instance_status(&run).unwrap().unwrap();
    let card = &desk.decision(&run).cards[0];
    assert_eq!(row.channel_id.as_deref(), Some(RUN_CHANNEL));
    assert_eq!(row.thread_id.as_deref(), Some(card.ts.as_str()));
}

#[tokio::test]
async fn an_undelivered_decision_is_posted_by_the_sweep_and_a_duplicate_is_removed() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    desk.runtime()
        .update_instance(&run, |i| {
            let decision = RunDecision::read(&i.variables).unwrap();
            RunDecision {
                posted: false,
                cards: Vec::new(),
                opened_at: chrono::Utc::now() - chrono::Duration::minutes(5),
                ..decision
            }
            .write(&mut i.variables);
            Ok::<_, eyre::Report>(())
        })
        .unwrap();
    let gate = desk.decision(&run).gate;

    tokio::join!(
        desk.runtime().decision_sweep(),
        desk.runtime().deliver_decision(&run, gate)
    );

    let decision = desk.decision(&run);
    assert!(decision.posted);
    assert_eq!(decision.cards.len(), 1);
    let deleted = desk.surface.deleted.lock().await;
    assert_eq!(deleted.len(), 1, "the losing delivery removes its card");
    assert_ne!(deleted[0].1, decision.cards[0].ts);
}

#[tokio::test]
async fn a_decision_settled_before_its_cards_were_recorded_closes_them_at_once() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    desk.runtime()
        .update_instance(&run, |i| {
            let decision = RunDecision::read(&i.variables).unwrap();
            RunDecision {
                posted: false,
                cards: Vec::new(),
                ..decision
            }
            .write(&mut i.variables);
            run_decision::stop(i, Some("Alice".into()), chrono::Utc::now());
            Ok::<_, eyre::Report>(())
        })
        .unwrap();
    let gate = desk.decision(&run).gate;

    desk.runtime().deliver_decision(&run, gate).await;

    let decision = desk.decision(&run);
    assert!(decision.posted && decision.cards_closed);
    let closed = desk.closed_cards().await;
    assert_eq!(card_count(&closed, "Stopped by Alice"), 1);
}

#[tokio::test]
async fn deciders_resolve_from_ids_emails_teams_and_mentions_and_fail_naming_the_bad_entry() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let hub = &desk.hub;
    link(hub, &hub.bob, Some("bob@example.com"), None);
    let erin = person(hub, "Erin", Some("erin@example.com"), Some("U-ERIN"));
    let frank = person(hub, "Frank", None, Some("U-FRANK"));
    let gone = person(hub, "Gone", Some("gone@example.com"), None);
    let mut suspended = store(hub)
        .get_by_id(uuid::Uuid::parse_str(&gone).unwrap())
        .unwrap()
        .unwrap();
    suspended.status = IdentityStatus::Suspended;
    store(hub).update(&suspended).unwrap();
    let teams = Arc::new(workforce_identity::sqlite::SqliteIdentityStore::in_memory().unwrap());
    let mut reviewers = workforce_identity::team::Team::new(
        "reviewers",
        uuid::Uuid::parse_str(&hub.alice).unwrap(),
    );
    reviewers.members = vec![
        uuid::Uuid::parse_str(&erin).unwrap(),
        uuid::Uuid::parse_str(&gone).unwrap(),
    ];
    workforce_identity::team::TeamStore::create_team(teams.as_ref(), &reviewers).unwrap();
    hub.service.set_team_store(teams);
    desk.surface
        .slack_users
        .lock()
        .insert("guest@example.com".into(), "U-GUEST".into());
    let surface: Arc<dyn ChannelSurface> = desk.surface.clone();
    let tokens = |list: &[&str]| list.iter().map(|t| t.to_string()).collect::<Vec<_>>();

    let deciders = desk
        .runtime()
        .resolve_deciders(
            &tokens(&[
                &hub.alice,
                "bob@example.com",
                "team:reviewers",
                "<@U-FRANK>",
                "<@U-HENRY>",
                "guest@example.com",
                "gone@example.com",
                &erin,
            ]),
            Some(&surface),
        )
        .await
        .unwrap();

    let named: Vec<(&str, Option<&str>)> = deciders
        .iter()
        .map(|d| (d.display.as_str(), d.slack_user_id.as_deref()))
        .collect();
    assert_eq!(
        named,
        [
            ("Alice", None),
            ("Bob", None),
            ("Erin", Some("U-ERIN")),
            ("Frank", Some("U-FRANK")),
            ("<@U-HENRY>", Some("U-HENRY")),
            ("guest@example.com", Some("U-GUEST")),
        ]
    );
    assert_eq!(deciders[3].identity_id.as_deref(), Some(frank.as_str()));

    let unknown = desk
        .runtime()
        .resolve_deciders(&tokens(&["nobody@example.com"]), Some(&surface))
        .await
        .unwrap_err();
    assert_eq!(
        unknown,
        "nobody has the email `nobody@example.com` in Workforce or in Slack"
    );
    let no_team = desk
        .runtime()
        .resolve_deciders(&tokens(&["team:nobody"]), Some(&surface))
        .await
        .unwrap_err();
    assert!(no_team.contains("`team:nobody`"), "{no_team}");
}

#[tokio::test]
async fn an_unlinked_email_decider_decides_once_slack_links_them() {
    let desk = Desk::new(|_| "erin@example.com".into(), "").await;
    desk.surface
        .slack_users
        .lock()
        .insert("erin@example.com".into(), "U-ERIN".into());
    let run = desk.waiting().await;
    assert_eq!(desk.decision(&run).deciders[0].identity_id, None);
    let erin = person(&desk.hub, "Erin", None, Some("U-ERIN"));

    assert!(desk.press(&run, &member(&erin), "send").await.is_ok());
}

#[tokio::test]
async fn a_decider_merged_into_another_identity_still_decides() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    link(&desk.hub, &desk.hub.bob, Some("bob@example.com"), None);
    let run = desk.waiting().await;
    let merged = person(&desk.hub, "Robert", Some("Bob@Example.com"), None);

    assert!(desk.press(&run, &member(&merged), "send").await.is_ok());
}

#[tokio::test]
async fn an_operator_variable_write_racing_a_settlement_never_reopens_the_gate() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    let stale = desk.runtime().get_instance_status(&run).unwrap().unwrap();
    settle_only(&desk, &run, "send");

    desk.runtime()
        .set_operator_variable(
            &workforce_common::session::TurnPrincipal::run_as(Some(desk.hub.alice.clone())),
            &run,
            "note",
            "checked",
        )
        .unwrap();
    let rewrote = desk
        .hub
        .service
        .telemetry_store
        .save_workflow_instance_fenced(&stale, stale.version)
        .unwrap();

    assert!(!rewrote, "a write from before the settlement is refused");
    let instance = desk.instance(&run);
    assert_eq!(instance.variables["note"], "checked");
    assert!(run_decision::open_in(&instance).is_none());
    assert!(run_decision::awaiting_follow(&instance).is_some());
}

#[tokio::test]
async fn a_loop_exiting_as_a_settlement_lands_hands_the_run_on() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    desk.runtime().active_loops.lock().insert(run.clone());
    desk.press(&run, &desk.bob(), "send").await.unwrap();
    assert_eq!(
        desk.instance(&run).status,
        InstanceStatus::Running,
        "no loop took it"
    );
    desk.runtime().active_loops.lock().remove(&run);

    desk.runtime().hand_on(&run);

    assert_eq!(desk.hub.ran(&run).await, ["draft", "review", "send"]);
}

#[tokio::test]
async fn a_decider_can_see_the_run_they_decide() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    let reader = |id: &str| crate::def_write_auth::DefWriter {
        identity_id: Some(id.to_string()),
        is_admin: false,
    };

    assert!(desk
        .runtime()
        .readable_run(&run, &reader(&desk.hub.bob))
        .await
        .unwrap()
        .is_some());
    assert!(desk
        .runtime()
        .readable_run(&run, &reader(&desk.dan))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn the_decision_view_says_who_it_waits_on_and_whether_the_viewer_may_decide() {
    let desk = Desk::new(
        |hub| hub.bob.clone(),
        "expires_after = \"20h\"\non_expiry = \"cancel\"",
    )
    .await;
    let run = desk.waiting().await;
    let row = desk.runtime().get_instance_status(&run).unwrap().unwrap();

    let view = desk
        .runtime()
        .decision_view(&row, Some(&desk.hub.bob))
        .unwrap();
    assert_eq!(view["title"], "Review the digest");
    assert_eq!(view["waiting_on"], serde_json::json!(["Bob"]));
    assert_eq!(view["on_expiry"], "Cancel");
    assert_eq!(view["may_decide"], true);
    assert_eq!(view["choices"][0]["style"], "primary");
    assert_eq!(view["gate"], desk.decision(&run).gate.to_string());
    let alice = desk
        .runtime()
        .decision_view(&row, Some(&desk.hub.alice))
        .unwrap();
    assert_eq!(alice["may_decide"], false);

    desk.press(&run, &desk.bob(), "send").await.unwrap();
    desk.hub.settled(&run).await;
    let row = desk.runtime().get_instance_status(&run).unwrap().unwrap();
    assert!(desk
        .runtime()
        .decision_view(&row, Some(&desk.hub.bob))
        .is_none());
}

#[tokio::test]
async fn resuming_through_any_surface_presses_the_open_decision() {
    let desk = Desk::new(|hub| hub.bob.clone(), "").await;
    let run = desk.waiting().await;
    let ask = |outcome: Option<&str>, gate: Option<uuid::Uuid>| ResumeAsk {
        outcome: outcome.map(str::to_string),
        gate,
        feedback: None,
        channel_id: None,
        thread_id: None,
        originating_agent_id: None,
    };

    let missing = desk
        .runtime()
        .resume_as(&run, ask(None, None), &desk.bob())
        .await;
    assert!(matches!(
        missing,
        Err(ResumeRefused::Decision(DecisionRefused::Choose { ref choices })) if choices == "Send · Redraft · Cancel · Done · Flag"
    ));
    let stale = desk
        .runtime()
        .resume_as(
            &run,
            ask(Some("send"), Some(uuid::Uuid::new_v4())),
            &desk.bob(),
        )
        .await;
    assert!(matches!(
        stale,
        Err(ResumeRefused::Decision(DecisionRefused::Outdated))
    ));
    let decided = desk
        .runtime()
        .resume_as(&run, ask(Some("send"), None), &desk.bob())
        .await;
    assert!(matches!(decided, Ok(Resumed::Decided(ref made)) if made.outcome == "send"));
    assert_eq!(desk.hub.ran(&run).await, ["draft", "review", "send"]);
}
