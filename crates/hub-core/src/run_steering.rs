//! Who may steer a workflow run that is under way — resume it, approve or
//! reject it, send it to a step, skip or retry one, pause it or stop it — on
//! every surface that can (Slack thread commands, the admin REST routes, the
//! `workflow` tool), decided once.
//!
//! A run acts as the person who started it whoever steers it, so steering is
//! never a way to act as anyone. Its starter — the person it acts as, or for a
//! run in an agent's or the system's lane the person it is billed to — may
//! always steer it, and so may an administrator.
//! Anyone else may when the workflow reaches them — it is everyone's, their
//! team's, or shared with them — and its definition lets others steer its
//! runs (`steerable_by_others`, on by default). Someone else steers the run
//! where it already reports: only its starter and administrators may send
//! what it produces anywhere new.
//!
//! The terms bind as the run started under them unless today's are stricter:
//! a workflow made private, or its flag switched off, binds the runs already
//! under way, while one widened mid-run does not widen them. Who it is shared
//! with is read live: a share is a grant on the row that its owner may take
//! back at any time, not part of any revision.
//!
//! Every decision, allowed or refused, is recorded against the person who
//! asked ([`recorded`]); what a permitted steer did to the run is also kept
//! with the run's other interventions
//! ([`crate::decision_records::moved`]).

use workforce_common::session::TurnPrincipal;
use workforce_core::access::Visibility;
use workforce_memory::access_audit::{emit, AccessAuditEvent, AuditDecision, AuditOperation};
use workforce_orchestrator::workflow::instance::Actor;

use crate::def_write_auth::{member_identity, DefWriter};

pub use workforce_orchestrator::workflow::instance::STEERING_KEY;

/// What steering does to a run, for the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerAction {
    Resume,
    Reject,
    Goto,
    Skip,
    Retry,
    Pause,
    Abort,
    /// Pressing a button of the decision a run waits on.
    Decide,
}

impl SteerAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Reject => "reject",
            Self::Goto => "goto",
            Self::Skip => "skip",
            Self::Retry => "retry",
            Self::Pause => "pause",
            Self::Abort => "abort",
            Self::Decide => "decide",
        }
    }
}

/// The person asking to steer a run, as their surface knows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Steerer {
    pub writer: DefWriter,
    /// Who the run's history names as having stepped in.
    pub actor: Actor,
}

/// Why a steer was allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// The person the run is done for.
    Starter,
    Admin,
    /// Someone the workflow reaches, while its terms let others steer.
    Other,
    /// Someone the decision a run waits on names.
    Decider,
}

impl Standing {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Starter => "starter",
            Self::Admin => "admin",
            Self::Other => "other",
            Self::Decider => "decider",
        }
    }
}

/// Leave to steer one run. Only [`crate::workflow_runtime::WorkflowRuntime::authorize_steer`]
/// grants one to a person, recording the decision as it does, and every
/// method that moves a run takes one — so a surface cannot move a run
/// without having asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Steer {
    actor: Actor,
    redirects: bool,
}

impl Steer {
    pub fn granted(steerer: &Steerer, standing: Standing) -> Self {
        Self {
            actor: steerer.actor.clone(),
            redirects: standing != Standing::Other,
        }
    }

    /// The hub moving a run of its own accord: the maintenance sweep.
    pub fn hub() -> Self {
        Self {
            actor: Actor::System,
            redirects: true,
        }
    }

    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// Whether this steer may send the run's reports somewhere new. Anyone
    /// but its starter or an administrator steers it where it already
    /// reports, so what it read on its starter's behalf never lands in a
    /// place of theirs.
    pub fn redirects_delivery(&self) -> bool {
        self.redirects
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn by(actor: Actor) -> Self {
        Self {
            actor,
            redirects: true,
        }
    }
}

/// The steering terms a workflow sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SteerTerms {
    pub by_others: bool,
    pub visibility: Visibility,
}

impl SteerTerms {
    /// The terms that bind a run: those it `started` under, narrowed by
    /// today's. A run started before terms were kept is bound by today's.
    pub fn binding(started: Option<Self>, now: Self) -> Self {
        let Some(started) = started else {
            return now;
        };
        Self {
            by_others: started.by_others && now.by_others,
            visibility: narrower(started.visibility, now.visibility),
        }
    }

    pub fn to_value(self) -> serde_json::Value {
        serde_json::json!({
            "by_others": self.by_others,
            "visibility": self.visibility.as_str(),
        })
    }

    /// The terms kept in `variables`, if any were. A value that does not read
    /// is kept as the closed terms, never as none.
    pub fn kept_in(variables: &serde_json::Value) -> Option<Self> {
        let kept = variables.get(STEERING_KEY)?;
        Some(Self {
            by_others: kept
                .get("by_others")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            visibility: kept
                .get("visibility")
                .and_then(serde_json::Value::as_str)
                .and_then(Visibility::parse)
                .unwrap_or(Visibility::Private),
        })
    }
}

fn narrower(a: Visibility, b: Visibility) -> Visibility {
    let reach = |v| match v {
        Visibility::Private => 0,
        Visibility::Team => 1,
        Visibility::Global => 2,
    };
    if reach(a) <= reach(b) {
        a
    } else {
        b
    }
}

/// A run's workflow as the rule reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunWorkflow {
    /// The workflow's own id, as its people know it.
    pub id: String,
    pub terms: SteerTerms,
}

/// Why a steer was refused. The message is what the person sees: one
/// sentence naming why, and what they can do instead.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SteerRefused {
    #[error(
        "Only the person who started this run, or an administrator, can steer it, and you are \
         not signed in as anyone; link your account and try again."
    )]
    Anonymous,
    #[error(
        "Only the person who started this run, or an administrator, can steer it now that its \
         workflow is no longer there."
    )]
    Gone,
    #[error(
        "Only the person who started this run, or an administrator, can steer it, because \
         `{workflow}` is a private workflow."
    )]
    Private { workflow: String },
    #[error(
        "Only the person who started this run, or an administrator, can steer it, because \
         `{workflow}` is not shared with you."
    )]
    NotShared { workflow: String },
    #[error(
        "Only the person who started this run, or an administrator, can steer it, because \
         `{workflow}` does not let others steer its runs; start your own run of `{workflow}` \
         instead."
    )]
    Closed { workflow: String },
    #[error("Could not check who may steer this run: {0}")]
    Unknown(String),
    #[error("Only {deciders} can decide this.")]
    NotDecider { deciders: String },
    #[error("You started this run, so this decision is for {deciders}.")]
    OwnDecision { deciders: String },
    #[error(
        "This run is waiting on a decision ({choices}) that only {deciders} can make; meanwhile \
         it can only be stopped."
    )]
    AwaitingDecision { choices: String, deciders: String },
    #[error(
        "Only the person who started this run, or an administrator, can stop it while it waits \
         on a decision."
    )]
    StopAtDecision,
}

/// The person a run is done for: whoever it acts as, or, when it runs in a
/// `system:` or `agent:` lane, whoever it is billed to.
pub fn starter(done_for: &TurnPrincipal) -> Option<&str> {
    person(done_for.run_as.as_deref()).or_else(|| person(done_for.bill_to.as_deref()))
}

/// The standing a steerer holds without looking at the workflow: the run's
/// starter, or an administrator. A run done for nobody has no starter to
/// match, and neither does a steerer who is nobody.
pub fn own_standing(steerer: &DefWriter, done_for: &TurnPrincipal) -> Option<Standing> {
    let me = member_identity(steerer.identity_id.as_deref());
    if me.is_some() && me == starter(done_for) {
        Some(Standing::Starter)
    } else if steerer.is_admin {
        Some(Standing::Admin)
    } else {
        None
    }
}

/// The rule. `reaches` is whether the run's workflow reaches the steerer
/// under the binding visibility (`may_read_def`), asked only when they hold
/// no standing of their own. Steering a run that acts as a person takes a
/// signed-in member, as starting one does; a run done for nobody (a system
/// run) is open to whoever its workflow reaches.
pub fn decide(
    steerer: &DefWriter,
    done_for: &TurnPrincipal,
    workflow: Option<&RunWorkflow>,
    reaches: bool,
) -> Result<Standing, SteerRefused> {
    if let Some(standing) = own_standing(steerer, done_for) {
        return Ok(standing);
    }
    if member_identity(steerer.identity_id.as_deref()).is_none() && starter(done_for).is_some() {
        return Err(SteerRefused::Anonymous);
    }
    let workflow = workflow.ok_or(SteerRefused::Gone)?;
    let named = || workflow.id.clone();
    match (reaches, workflow.terms) {
        (
            false,
            SteerTerms {
                visibility: Visibility::Private,
                ..
            },
        ) => Err(SteerRefused::Private { workflow: named() }),
        (false, _) => Err(SteerRefused::NotShared { workflow: named() }),
        (
            true,
            SteerTerms {
                by_others: false, ..
            },
        ) => Err(SteerRefused::Closed { workflow: named() }),
        (true, _) => Ok(Standing::Other),
    }
}

/// The person `id` names: none for an empty id, the hub's own
/// pseudo-identities, or a `system:` or `agent:` lane.
fn person(id: Option<&str>) -> Option<&str> {
    member_identity(id).filter(|id| !id.starts_with("system:") && !id.starts_with("agent:"))
}

/// Record that `steerer` asked to steer `run`, done for `done_for`, and what
/// they were told.
pub fn recorded(
    run: &str,
    done_for: &TurnPrincipal,
    steerer: &Steerer,
    action: SteerAction,
    verdict: &Result<Standing, SteerRefused>,
) {
    let requester = member_identity(steerer.writer.identity_id.as_deref());
    let starter = starter(done_for);
    let decision = match verdict {
        Ok(_) => AuditDecision::Allow,
        Err(_) => AuditDecision::DeniedOwnerMismatch,
    };
    emit(AccessAuditEvent::new(
        AuditOperation::WorkflowRunSteer,
        decision,
        requester,
        starter,
        Some(&format!("workflow_run:{run}")),
    ));
    tracing::info!(
        target: "audit.workflow_run_steer",
        run = %run,
        action = action.as_str(),
        actor = ?steerer.actor,
        starter = ?starter,
        standing = verdict.as_ref().ok().map(|s| s.as_str()),
        refused = verdict.as_ref().err().map(tracing::field::display),
        "Workflow run steer"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: &str = "11111111-1111-4111-8111-111111111111";
    const BOB: &str = "22222222-2222-4222-8222-222222222222";

    fn ran_as(id: &str) -> TurnPrincipal {
        TurnPrincipal::run_as(Some(id.into()))
    }

    fn member(id: &str) -> DefWriter {
        DefWriter {
            identity_id: Some(id.into()),
            is_admin: false,
        }
    }

    fn admin(id: &str) -> DefWriter {
        DefWriter {
            identity_id: Some(id.into()),
            is_admin: true,
        }
    }

    fn workflow(visibility: Visibility, by_others: bool) -> RunWorkflow {
        RunWorkflow {
            id: "deploy".into(),
            terms: SteerTerms {
                by_others,
                visibility,
            },
        }
    }

    #[test]
    fn the_starter_may_steer_their_run_whatever_its_workflow_says() {
        for terms in [
            Some(workflow(Visibility::Private, false)),
            Some(workflow(Visibility::Global, false)),
            None,
        ] {
            assert_eq!(
                decide(&member(ALICE), &ran_as(ALICE), terms.as_ref(), false),
                Ok(Standing::Starter)
            );
        }
    }

    #[test]
    fn an_admin_may_steer_anyones_run_whatever_its_workflow_says() {
        for terms in [Some(workflow(Visibility::Private, false)), None] {
            assert_eq!(
                decide(&admin(BOB), &ran_as(ALICE), terms.as_ref(), false),
                Ok(Standing::Admin)
            );
        }
    }

    #[test]
    fn another_member_may_steer_a_run_of_a_workflow_that_reaches_them_while_others_may() {
        for visibility in [Visibility::Global, Visibility::Team, Visibility::Private] {
            assert_eq!(
                decide(
                    &member(BOB),
                    &ran_as(ALICE),
                    Some(&workflow(visibility, true)),
                    true
                ),
                Ok(Standing::Other),
                "{visibility:?}: everyone's, their team's, or shared with them"
            );
        }
    }

    #[test]
    fn another_member_may_not_steer_once_the_workflow_says_others_may_not() {
        assert_eq!(
            decide(
                &member(BOB),
                &ran_as(ALICE),
                Some(&workflow(Visibility::Global, false)),
                true
            ),
            Err(SteerRefused::Closed {
                workflow: "deploy".into()
            })
        );
    }

    #[test]
    fn another_member_may_not_steer_a_run_of_a_private_workflow() {
        assert_eq!(
            decide(
                &member(BOB),
                &ran_as(ALICE),
                Some(&workflow(Visibility::Private, true)),
                false
            ),
            Err(SteerRefused::Private {
                workflow: "deploy".into()
            })
        );
        assert_eq!(
            decide(
                &member(BOB),
                &ran_as(ALICE),
                Some(&workflow(Visibility::Team, true)),
                false
            ),
            Err(SteerRefused::NotShared {
                workflow: "deploy".into()
            }),
            "a team workflow outside their team"
        );
    }

    #[test]
    fn nobody_signed_in_may_steer_a_persons_run_but_may_steer_a_system_run_it_reaches() {
        let nobody = DefWriter::default();
        let open = workflow(Visibility::Global, true);
        assert_eq!(
            decide(&nobody, &ran_as(ALICE), Some(&open), true),
            Err(SteerRefused::Anonymous)
        );
        assert_eq!(
            decide(&nobody, &TurnPrincipal::default(), Some(&open), true),
            Ok(Standing::Other)
        );
        assert_eq!(
            decide(&nobody, &ran_as("system:sweep"), Some(&open), true),
            Ok(Standing::Other),
            "a lane is nobody"
        );
        assert_eq!(
            own_standing(&nobody, &TurnPrincipal::default()),
            None,
            "nobody is not nobody's starter"
        );
    }

    #[test]
    fn the_person_a_lane_run_is_billed_to_may_steer_it_as_its_starter() {
        let billed = TurnPrincipal {
            run_as: Some("agent:triage".into()),
            bill_to: Some(ALICE.into()),
        };
        assert_eq!(
            decide(
                &member(ALICE),
                &billed,
                Some(&workflow(Visibility::Global, false)),
                true
            ),
            Ok(Standing::Starter)
        );
        assert_eq!(
            decide(
                &member(BOB),
                &billed,
                Some(&workflow(Visibility::Global, false)),
                true
            ),
            Err(SteerRefused::Closed {
                workflow: "deploy".into()
            })
        );
    }

    #[test]
    fn a_person_run_billed_to_someone_else_is_still_its_runners_alone() {
        let billed = TurnPrincipal {
            run_as: Some(ALICE.into()),
            bill_to: Some(BOB.into()),
        };
        assert_eq!(
            decide(
                &member(BOB),
                &billed,
                Some(&workflow(Visibility::Global, false)),
                true
            ),
            Err(SteerRefused::Closed {
                workflow: "deploy".into()
            })
        );
    }

    #[test]
    fn only_the_starter_and_admins_may_move_a_runs_reports() {
        let bob = Steerer {
            writer: member(BOB),
            actor: Actor::Person { id: BOB.into() },
        };
        assert!(Steer::granted(&bob, Standing::Starter).redirects_delivery());
        assert!(Steer::granted(&bob, Standing::Admin).redirects_delivery());
        assert!(!Steer::granted(&bob, Standing::Other).redirects_delivery());
    }

    #[test]
    fn a_run_whose_workflow_is_gone_is_left_to_its_starter_and_administrators() {
        assert_eq!(
            decide(&member(BOB), &ran_as(ALICE), None, false),
            Err(SteerRefused::Gone)
        );
    }

    #[test]
    fn the_refusal_names_why_and_what_to_do_in_one_sentence() {
        let closed = SteerRefused::Closed {
            workflow: "deploy".into(),
        }
        .to_string();
        assert_eq!(
            closed,
            "Only the person who started this run, or an administrator, can steer it, because \
             `deploy` does not let others steer its runs; start your own run of `deploy` instead."
        );
        for refused in [
            SteerRefused::Anonymous,
            SteerRefused::Gone,
            SteerRefused::Private {
                workflow: "deploy".into(),
            },
            SteerRefused::NotShared {
                workflow: "deploy".into(),
            },
        ] {
            let said = refused.to_string();
            assert_eq!(said.matches(". ").count(), 0, "{said}");
            assert!(said.ends_with('.'), "{said}");
        }
    }

    #[test]
    fn a_run_is_bound_by_the_stricter_of_the_terms_it_started_under_and_todays() {
        let open = SteerTerms {
            by_others: true,
            visibility: Visibility::Global,
        };
        let closed = SteerTerms {
            by_others: false,
            visibility: Visibility::Private,
        };
        assert_eq!(
            SteerTerms::binding(Some(open), closed),
            closed,
            "tightened mid-run"
        );
        assert_eq!(
            SteerTerms::binding(Some(closed), open),
            closed,
            "widened mid-run"
        );
        assert_eq!(
            SteerTerms::binding(None, open),
            open,
            "started before terms were kept"
        );
        let team = SteerTerms {
            by_others: true,
            visibility: Visibility::Team,
        };
        assert_eq!(
            SteerTerms::binding(Some(open), team).visibility,
            Visibility::Team
        );
    }

    #[test]
    fn kept_terms_read_back_and_unreadable_ones_read_as_closed() {
        let team = SteerTerms {
            by_others: true,
            visibility: Visibility::Team,
        };
        let vars = serde_json::json!({ STEERING_KEY: team.to_value() });
        assert_eq!(SteerTerms::kept_in(&vars), Some(team));
        assert_eq!(SteerTerms::kept_in(&serde_json::json!({})), None);
        assert_eq!(
            SteerTerms::kept_in(&serde_json::json!({ STEERING_KEY: "garbled" })),
            Some(SteerTerms {
                by_others: false,
                visibility: Visibility::Private
            })
        );
    }

    #[test]
    fn decision_refusals_name_the_deciders() {
        let deciders = || "Alice, Bob or Ana".to_string();
        assert_eq!(
            SteerRefused::NotDecider {
                deciders: deciders()
            }
            .to_string(),
            "Only Alice, Bob or Ana can decide this."
        );
        assert_eq!(
            SteerRefused::OwnDecision {
                deciders: deciders()
            }
            .to_string(),
            "You started this run, so this decision is for Alice, Bob or Ana."
        );
        assert_eq!(
            SteerRefused::AwaitingDecision {
                choices: "Send · Cancel".into(),
                deciders: deciders()
            }
            .to_string(),
            "This run is waiting on a decision (Send · Cancel) that only Alice, Bob or Ana can \
             make; meanwhile it can only be stopped."
        );
        assert!(SteerRefused::StopAtDecision
            .to_string()
            .contains("while it waits on a decision"));
    }

    #[test]
    fn a_decide_is_recorded_allowed_and_refused() {
        use workforce_memory::access_audit;
        let buffer = access_audit::install_ring_buffer(256);
        let _ = access_audit::set_global_sink(buffer.clone());
        let run = uuid::Uuid::new_v4().to_string();
        let bob = Steerer {
            writer: member(BOB),
            actor: Actor::Person { id: BOB.into() },
        };
        recorded(
            &run,
            &ran_as(ALICE),
            &bob,
            SteerAction::Decide,
            &Ok(Standing::Decider),
        );
        recorded(
            &run,
            &ran_as(ALICE),
            &bob,
            SteerAction::Decide,
            &Err(SteerRefused::NotDecider {
                deciders: "Carol".into(),
            }),
        );
        let resource = format!("workflow_run:{run}");
        let decisions: Vec<_> = buffer
            .snapshot()
            .into_iter()
            .filter(|e| e.resource_id.as_deref() == Some(resource.as_str()))
            .map(|e| e.decision)
            .collect();
        assert_eq!(
            decisions,
            [AuditDecision::DeniedOwnerMismatch, AuditDecision::Allow]
        );
        assert_eq!(SteerAction::Decide.as_str(), "decide");
        assert_eq!(Standing::Decider.as_str(), "decider");
    }

    #[test]
    fn every_decision_is_recorded_against_the_person_who_asked() {
        use workforce_memory::access_audit;
        let buffer = access_audit::install_ring_buffer(256);
        let _ = access_audit::set_global_sink(buffer.clone());
        let run = uuid::Uuid::new_v4().to_string();
        let bob = Steerer {
            writer: member(BOB),
            actor: Actor::Person { id: BOB.into() },
        };
        recorded(
            &run,
            &ran_as(ALICE),
            &bob,
            SteerAction::Abort,
            &Ok(Standing::Other),
        );
        recorded(
            &run,
            &ran_as(ALICE),
            &bob,
            SteerAction::Resume,
            &Err(SteerRefused::Gone),
        );
        let resource = format!("workflow_run:{run}");
        let kept: Vec<_> = buffer
            .snapshot()
            .into_iter()
            .filter(|e| e.resource_id.as_deref() == Some(resource.as_str()))
            .map(|e| {
                (
                    e.operation,
                    e.decision,
                    e.requester_identity_id,
                    e.owner_identity_id,
                )
            })
            .collect();
        let named = |decision| {
            (
                AuditOperation::WorkflowRunSteer,
                decision,
                Some(BOB.to_string()),
                Some(ALICE.to_string()),
            )
        };
        assert_eq!(
            kept,
            [
                named(AuditDecision::DeniedOwnerMismatch),
                named(AuditDecision::Allow)
            ],
            "newest first"
        );
    }
}
