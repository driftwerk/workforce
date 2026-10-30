//! Agents' turns, written to the log as the turn host runs them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use uuid::Uuid;
use workforce_agent::runner::ProgressEvent;
use workforce_common::channels::desk::{DeliveryKind, DeskPlace};
use workforce_common::subscription::personalizes_prompt;
use workforce_desk::crew;
use workforce_desk::floor::{
    Answer, DecideRef, Outcome, Place, Subject, Turn, TurnLog, TurnOp, Wait,
};

use crate::desk_crew::CHECK_IN;
use crate::desk_floor::{by_person_of, place_in_desk, place_of, Origin};

/// How often a running turn's progress is written.
const PROGRESS_EVERY: Duration = Duration::from_secs(15);

/// One agent turn being logged; dropped before it ended, it was stopped.
pub(crate) struct AgentTurn {
    log: Arc<dyn TurnLog>,
    id: Uuid,
    agent_id: String,
    session_id: String,
    ended: bool,
}

/// What a running turn reports as it runs: its progress, no more often than
/// [`PROGRESS_EVERY`], and each prompt it waits on the person for.
pub(crate) struct Heard {
    log: Arc<dyn TurnLog>,
    id: Uuid,
    agent_id: String,
    session_id: String,
    last: Option<Instant>,
}

/// A turn of `agent_id`'s in `session_id`, for `owner`, opened at `at`.
fn opened(
    id: Uuid,
    owner: &str,
    agent_id: &str,
    session_id: &str,
    name: String,
    place: Place,
    at: DateTime<Utc>,
) -> Turn {
    Turn {
        id,
        owner: Some(owner.to_string()),
        subject: Subject::Agent {
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
        },
        name,
        place,
        task: None,
        doing: None,
        started_at: at,
        heard_at: at,
        ended_at: None,
        outcome: None,
        answer: None,
        question: None,
        settled_at: None,
        waits: vec![],
        idle: vec![],
        token: None,
        instance: None,
    }
}

/// A check-in delivered to `owner`'s Desk conversation `session_id` with
/// `agent_id` at `place`, logged as a turn begun and ended as it landed: one
/// that asks the person something needs them. No other delivery is logged:
/// a job's, a workflow step's and a resumed turn's answer are their logged
/// turn's, a reminder asks nothing, and an approval is its own need.
pub(crate) fn delivered(
    log: &dyn TurnLog,
    agent_id: &str,
    owner: &str,
    session_id: &str,
    place: &DeskPlace,
    kind: DeliveryKind,
    text: &str,
) {
    if kind != DeliveryKind::CheckIn {
        return;
    }
    let (id, at) = (Uuid::new_v4(), crew::now());
    log.send(TurnOp::Begin {
        turn: Box::new(opened(
            id,
            owner,
            agent_id,
            session_id,
            agent_id.to_string(),
            place_in_desk(place),
            at,
        )),
        by_person: false,
    });
    let (outcome, answer, question) = crew::outcome_of(Ok(text));
    log.send(TurnOp::End {
        turn: id,
        at,
        outcome,
        answer,
        question,
    });
}

impl AgentTurn {
    /// Logs a turn of agent `agent_id` (called `name`) that began at
    /// `started_at`, unless it is housekeeping: a heartbeat, a monitor,
    /// Desk's own check-in. `token` is the session's turn token, when the
    /// turn opened a record; the logged turn takes it as its id.
    pub(crate) fn begin(
        log: &Arc<dyn TurnLog>,
        agent_id: &str,
        name: String,
        origin: &Origin<'_>,
        session_id: Option<&str>,
        token: Option<&str>,
        started_at: DateTime<Utc>,
    ) -> Option<Self> {
        if !personalizes_prompt(origin.user) || origin.message.starts_with(CHECK_IN) {
            return None;
        }
        let token = token.filter(|t| !t.is_empty());
        let id = token
            .and_then(|t| Uuid::parse_str(t).ok())
            .unwrap_or_else(Uuid::new_v4);
        let session_id = session_id.unwrap_or_default().to_string();
        let turn = Turn {
            task: crate::session_digest::first_sentence(origin.message),
            token: token.map(str::to_string),
            instance: Some(workforce_common::instance::process_instance_id().to_string()),
            ..opened(
                id,
                origin.owner(),
                agent_id,
                &session_id,
                name,
                place_of(origin),
                started_at,
            )
        };
        log.send(TurnOp::Begin {
            turn: Box::new(turn),
            by_person: by_person_of(origin),
        });
        Some(Self {
            log: Arc::clone(log),
            id,
            agent_id: agent_id.to_string(),
            session_id,
            ended: false,
        })
    }

    pub(crate) fn heard(&self) -> Heard {
        Heard {
            log: Arc::clone(&self.log),
            id: self.id,
            agent_id: self.agent_id.clone(),
            session_id: self.session_id.clone(),
            last: None,
        }
    }

    /// The turn ended with `answer`, or failed with it as `Err`.
    pub(crate) fn end(mut self, answer: Result<&str, &str>) {
        let (outcome, answer, question) = crew::outcome_of(answer);
        self.close(outcome, answer, question);
    }

    fn close(
        &mut self,
        outcome: Outcome,
        answer: Option<String>,
        question: Option<crew::ItemQuestion>,
    ) {
        self.ended = true;
        self.log.send(TurnOp::End {
            turn: self.id,
            at: crew::now(),
            outcome,
            answer,
            question,
        });
    }
}

impl Drop for AgentTurn {
    /// A turn that ended without an answer was stopped, unless the hub is
    /// stopping: its record survives for the successor to resume, and the
    /// row reads as interrupted once nothing runs it.
    fn drop(&mut self) {
        if !self.ended && !crate::shutdown::in_progress() {
            self.close(Outcome::Stopped, None, None);
        }
    }
}

impl Heard {
    pub(crate) fn saw(&mut self, event: &ProgressEvent) {
        let doing = match event {
            // A routed prompt is its own need, the approver's; only one the
            // person answers in the turn is this turn's wait.
            ProgressEvent::ApprovalRequired {
                request_id,
                tool_name,
                detail,
                routing: None,
                ..
            } => return self.waits_for(request_id, tool_name, detail),
            ProgressEvent::ApprovalResolved { request_id } => {
                return self.log.send(TurnOp::Unwait {
                    turn: self.id,
                    key: request_id.clone(),
                    at: crew::now(),
                })
            }
            ProgressEvent::ToolStart { name, detail, .. } => {
                Some(crew::clip(&format!("● {name} {detail}"), 160))
            }
            ProgressEvent::LlmCall { .. } => None,
            _ => return,
        };
        if self.last.is_some_and(|at| at.elapsed() < PROGRESS_EVERY) {
            return;
        }
        self.last = Some(Instant::now());
        self.log.send(TurnOp::Progress {
            turn: self.id,
            doing,
            at: crew::now(),
        });
    }

    fn waits_for(&self, request_id: &str, tool: &str, detail: &str) {
        self.log.send(TurnOp::Wait {
            turn: self.id,
            wait: Wait {
                key: request_id.to_string(),
                from: crew::now(),
                to: None,
                ask: crew::approval_summary(tool, detail),
                answer: Answer::Decide {
                    at: DecideRef::Prompt {
                        agent_id: self.agent_id.clone(),
                        session_id: self.session_id.clone(),
                        request_id: request_id.to_string(),
                    },
                },
            },
        });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use parking_lot::Mutex;
    use workforce_desk::floor::{Place, Scope};

    use super::*;

    /// A log that keeps every op, and reads them back by the log's rules.
    #[derive(Default)]
    pub(crate) struct Recorded(pub Mutex<Vec<TurnOp>>);

    #[async_trait::async_trait]
    impl TurnLog for Recorded {
        fn send(&self, op: TurnOp) {
            self.0.lock().push(op);
        }

        async fn relevant(&self, scope: &Scope, _: DateTime<Utc>) -> eyre::Result<Vec<Turn>> {
            Ok(self
                .0
                .lock()
                .iter()
                .fold(Vec::new(), workforce_desk::floor::apply)
                .into_iter()
                .filter(|t| scope.admits(t.owner.as_deref()))
                .collect())
        }
    }

    fn origin<'a>(user: &'a str, run_as: Option<&'a str>, message: &'a str) -> Origin<'a> {
        Origin {
            channel: "desk",
            user,
            run_as,
            message,
            channel_id: None,
            thread_id: None,
            desk: None,
            answers_to: None,
        }
    }

    fn begin(log: &Arc<dyn TurnLog>, o: &Origin<'_>, token: Option<&str>) -> Option<AgentTurn> {
        AgentTurn::begin(
            log,
            "scout",
            "Scout".into(),
            o,
            Some("s1"),
            token,
            crew::now(),
        )
    }

    fn ops(log: &Recorded) -> Vec<TurnOp> {
        log.0.lock().clone()
    }

    #[test]
    fn a_turn_is_logged_as_the_persons_under_its_token() {
        let recorded = Arc::new(Recorded::default());
        let log: Arc<dyn TurnLog> = recorded.clone();
        let token = Uuid::new_v4().to_string();
        let turn = begin(
            &log,
            &origin("u1", None, "Deploy main. Then report."),
            Some(&token),
        )
        .expect("logged");
        let logged = ops(&recorded);
        let [TurnOp::Begin {
            turn: begun,
            by_person,
        }] = logged.as_slice()
        else {
            panic!("one begin")
        };
        assert!(*by_person);
        assert_eq!(begun.id.to_string(), token);
        assert_eq!(begun.owner.as_deref(), Some("u1"));
        assert_eq!(begun.place, Place::Dm);
        assert_eq!(begun.task.as_deref(), Some("Deploy main."));
        assert_eq!(begun.token.as_deref(), Some(token.as_str()));
        turn.end(Ok("Built it. Deploy to staging?"));
        let ended = ops(&recorded);
        let TurnOp::End {
            outcome, question, ..
        } = &ended[1]
        else {
            panic!("an end")
        };
        assert_eq!(*outcome, Outcome::Asked);
        assert_eq!(question.as_ref().unwrap().text, "Deploy to staging?");
    }

    #[test]
    fn a_job_runs_as_its_person_and_housekeeping_is_not_logged() {
        let recorded = Arc::new(Recorded::default());
        let log: Arc<dyn TurnLog> = recorded.clone();
        let job = begin(&log, &origin("scheduler", Some("u1"), "Daily spend."), None).unwrap();
        let logged = ops(&recorded);
        let [TurnOp::Begin { turn, by_person }] = logged.as_slice() else {
            panic!("one begin")
        };
        assert!(!by_person, "a job settles nothing");
        assert_eq!(turn.owner.as_deref(), Some("u1"));
        assert_eq!(turn.token, None);
        drop(job);
        assert!(
            matches!(
                ops(&recorded)[1],
                TurnOp::End {
                    outcome: Outcome::Stopped,
                    ..
                }
            ),
            "dropped before it ended, it was stopped"
        );
        assert!(begin(&log, &origin("heartbeat", None, "check"), None).is_none());
        assert!(begin(&log, &origin("monitor", None, "seen"), None).is_none());
        assert!(begin(
            &log,
            &origin("u1", None, &format!("{CHECK_IN} status")),
            None
        )
        .is_none());
        assert_eq!(ops(&recorded).len(), 2);
    }

    #[test]
    fn a_prompt_the_person_answers_in_the_turn_is_a_wait_until_it_resolves() {
        let recorded = Arc::new(Recorded::default());
        let log: Arc<dyn TurnLog> = recorded.clone();
        let turn = begin(&log, &origin("u1", None, "go"), None).unwrap();
        let mut heard = turn.heard();
        let required = |request_id: &str, routing| ProgressEvent::ApprovalRequired {
            request_id: request_id.into(),
            agent_id: "scout".into(),
            tool_name: "Bash".into(),
            detail: "npm run deploy".into(),
            reason: "deploys".into(),
            round: 1,
            options: Default::default(),
            routing,
        };
        heard.saw(&required("r1", None));
        heard.saw(&required(
            "r2",
            Some(workforce_agent::runner::ApprovalRouting {
                approver_user_ids: vec!["u2".into()],
                requester_user_id: "u1".into(),
            }),
        ));
        let logged = ops(&recorded);
        let waits: Vec<&Wait> = logged
            .iter()
            .filter_map(|op| match op {
                TurnOp::Wait { wait, .. } => Some(wait),
                _ => None,
            })
            .collect();
        assert_eq!(waits.len(), 1, "a routed prompt is the approver's need");
        assert_eq!(waits[0].key, "r1");
        assert_eq!(waits[0].ask, "Run npm run deploy?");
        assert_eq!(
            waits[0].answer,
            Answer::Decide {
                at: DecideRef::Prompt {
                    agent_id: "scout".into(),
                    session_id: "s1".into(),
                    request_id: "r1".into()
                }
            }
        );
        heard.saw(&ProgressEvent::ApprovalResolved {
            request_id: "r1".into(),
        });
        assert!(matches!(
            ops(&recorded).last(),
            Some(TurnOp::Unwait { key, .. }) if key == "r1"
        ));
        turn.end(Ok("Deployed."));
    }

    #[tokio::test]
    async fn a_check_in_that_asks_needs_the_person_and_no_other_delivery_is_logged() {
        let recorded = Recorded::default();
        let room = Uuid::new_v4();
        let place = DeskPlace::Room {
            room,
            name: "Release".into(),
            thread: None,
        };
        for kind in [
            DeliveryKind::Job,
            DeliveryKind::Workflow,
            DeliveryKind::Resumed,
            DeliveryKind::Reminder,
            DeliveryKind::Approval,
            DeliveryKind::Notice,
            DeliveryKind::File,
        ] {
            delivered(&recorded, "scout", "u1", "s1", &place, kind, "Ship it?");
        }
        assert!(recorded.0.lock().is_empty());
        delivered(
            &recorded,
            "scout",
            "u1",
            "s1",
            &place,
            DeliveryKind::CheckIn,
            "The deploy is stuck. Retry it?",
        );
        let scope = Scope {
            owner: "u1".into(),
            personal: false,
        };
        let turns = recorded.relevant(&scope, crew::now()).await.unwrap();
        let needs = workforce_desk::floor::open_needs(&turns);
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].ask, "Retry it?");
        assert_eq!(needs[0].place, Place::Room { room, thread: None });
    }

    #[test]
    fn progress_is_written_at_most_every_fifteen_seconds() {
        let recorded = Arc::new(Recorded::default());
        let log: Arc<dyn TurnLog> = recorded.clone();
        let turn = begin(&log, &origin("u1", None, "go"), None).unwrap();
        let mut heard = turn.heard();
        let tool = ProgressEvent::ToolStart {
            name: "Bash".into(),
            round: 1,
            detail: "cargo test".into(),
        };
        heard.saw(&tool);
        heard.saw(&ProgressEvent::LlmCall { round: 2 });
        heard.saw(&tool);
        let progress: Vec<_> = ops(&recorded)
            .into_iter()
            .filter_map(|op| match op {
                TurnOp::Progress { doing, .. } => Some(doing),
                _ => None,
            })
            .collect();
        assert_eq!(progress, [Some("● Bash cargo test".to_string())]);
        turn.end(Err("connection refused"));
    }
}
