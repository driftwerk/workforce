//! What one change of a session means: the single reading of a session's
//! before and after that its activity is recorded from.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::asking::{self, Asked, Reading};
use crate::crew::{self, ItemQuestion};
use crate::floor::{Answer, DecideRef, Outcome, Place, Subject, Turn, TurnOp, Wait};
use crate::model::{PendingApproval, SessionState, SessionView};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Transition<'a> {
    /// First seen.
    Started,
    /// The person's prompt set it working again.
    Prompted {
        prompt: &'a str,
    },
    /// The person's prompt reached it otherwise: its first, given at start,
    /// or one typed while it was busy.
    Spoke {
        prompt: &'a str,
    },
    Blocked {
        pending: &'a PendingApproval,
    },
    /// A request Desk could answer passed to the terminal.
    ToTerminal {
        pending: &'a PendingApproval,
    },
    /// A request it was waiting on was answered: through Desk, or in its
    /// terminal.
    Unblocked {
        pending: &'a PendingApproval,
        by_desk: bool,
    },
    Finished {
        reply: &'a str,
    },
    /// Its turn ended waiting on the person: on what `asked` says.
    Asked {
        reply: &'a str,
        asked: &'a Asked,
    },
    /// Its turn was ended from Desk before it finished.
    Halted,
    /// What it asked was left unanswered: it is done.
    Dismissed,
    /// Its turn is done but waits on `count` pieces of the work it
    /// started, `from` before (none: it no longer waits).
    Background {
        from: u32,
        count: u32,
    },
    Stopped {
        closed: bool,
    },
    Interrupted,
    Resumed,
    Tool {
        tool: &'a str,
    },
}

impl<'a> Transition<'a> {
    /// Everything `now` changed since `before`, in order; `decided` holds the
    /// requests Desk answered.
    pub(super) fn of(
        before: Option<&'a SessionView>,
        now: &'a SessionView,
        decided: &HashSet<Uuid>,
    ) -> Vec<Self> {
        use SessionState as S;
        let Some(before) = before else {
            return vec![Self::Started];
        };
        let pending_id = |v: &SessionView| v.pending.as_ref().map(|p| p.id);
        // Running here, each prompt is counted; elsewhere only its words
        // are seen.
        let prompted = match now.live {
            true => now.prompts != before.prompts,
            false => now.last_prompt != before.last_prompt,
        };
        let unblocked = before
            .pending
            .as_ref()
            .filter(|p| now.state.is_running() && pending_id(now) != Some(p.id))
            .map(|pending| Self::Unblocked {
                pending,
                by_desk: decided.contains(&pending.id),
            });
        let state = match now.state {
            S::Blocked if before.state != S::Blocked || pending_id(now) != pending_id(before) => {
                now.pending
                    .as_ref()
                    .map(|pending| Self::Blocked { pending })
            }
            S::Working
                if matches!(before.state, S::Done | S::Idle | S::Starting | S::Asking)
                    && prompted =>
            {
                now.last_prompt
                    .as_deref()
                    .map(|prompt| Self::Prompted { prompt })
            }
            S::Done if now.interrupted && !before.interrupted => Some(Self::Halted),
            S::Done if before.state == S::Asking => Some(Self::Dismissed),
            S::Asking if before.state != S::Asking => now.asked.as_ref().map(|asked| Self::Asked {
                reply: now.last_reply.as_deref().unwrap_or_default(),
                asked,
            }),
            S::Done
                if now.waiting_on() == 0
                    && (before.state != S::Done || before.waiting_on() > 0) =>
            {
                Some(Self::Finished {
                    reply: now.last_reply.as_deref().unwrap_or_default(),
                })
            }
            // Another terminal's session, gone quiet: its turn is done.
            S::Idle if matches!(before.state, S::Working | S::Blocked) => Some(Self::Finished {
                reply: now.last_reply.as_deref().unwrap_or_default(),
            }),
            S::Stopped | S::Closed if before.state.is_running() => Some(Self::Stopped {
                closed: now.state == S::Closed,
            }),
            S::Interrupted if before.state != S::Interrupted => Some(Self::Interrupted),
            S::Starting
                if matches!(
                    before.state,
                    S::Stopped | S::Interrupted | S::Closed | S::Done | S::Idle
                ) =>
            {
                Some(Self::Resumed)
            }
            _ => None,
        };
        let spoke = now
            .last_prompt
            .as_deref()
            .filter(|_| {
                !matches!(
                    state,
                    Some(
                        Self::Prompted { .. }
                            | Self::Finished { .. }
                            | Self::Asked { .. }
                            | Self::Halted
                            | Self::Stopped { .. }
                            | Self::Interrupted
                    )
                )
            })
            .filter(|_| {
                prompted
                    || (now.state == S::Working && matches!(before.state, S::Starting | S::Idle))
            })
            .map(|prompt| Self::Spoke { prompt });
        let to_terminal = now
            .pending
            .as_ref()
            .filter(|p| {
                !p.answerable
                    && before
                        .pending
                        .as_ref()
                        .is_some_and(|b| b.id == p.id && b.answerable)
            })
            .map(|pending| Self::ToTerminal { pending });
        let background = (now.waiting_on() != before.waiting_on()).then(|| Self::Background {
            from: before.waiting_on(),
            count: now.waiting_on(),
        });
        let tool = now
            .current_tool
            .as_deref()
            .filter(|_| now.state == S::Working && now.current_tool != before.current_tool)
            .map(|tool| Self::Tool { tool });
        unblocked
            .into_iter()
            .chain(background)
            .chain(state)
            .chain(spoke)
            .chain(to_terminal)
            .chain(tool)
            .collect()
    }
}

/// The turn log's writes for `transitions` of `now`, at `at`, given the turn
/// it has open; and the turn open after them. A turn begins on the person's
/// prompt, or (not theirs to settle anything) when work is seen without one.
pub(super) fn turn_ops(
    transitions: &[Transition<'_>],
    now: &SessionView,
    open: Option<Uuid>,
    at: DateTime<Utc>,
    mut fresh: impl FnMut() -> Uuid,
) -> (Vec<TurnOp>, Option<Uuid>) {
    let subject = Subject::Session {
        id: now.id,
        engine: now.engine,
    };
    let mut begin = |by_person: bool, task: Option<&str>| {
        let id = fresh();
        let turn = Turn {
            id,
            owner: None,
            subject: subject.clone(),
            name: now.label().to_string(),
            place: Place::Session,
            task: task
                .map(|t| crew::one_line(t, TASK_CHARS))
                .filter(|t| !t.is_empty()),
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
        };
        (
            TurnOp::Begin {
                turn: Box::new(turn),
                by_person,
            },
            id,
        )
    };
    let task = now
        .last_prompt
        .as_deref()
        .or(Some(now.goal.as_str()).filter(|g| !g.trim().is_empty()));
    let end = |turn: Uuid, (outcome, answer, question)| TurnOp::End {
        turn,
        at,
        outcome,
        answer,
        question,
    };
    let settle = || TurnOp::Settle {
        subject: subject.key(),
        owner: None,
        at,
    };
    let mut ops = Vec::new();
    let mut open = open;
    for t in transitions {
        let mut working = |ops: &mut Vec<TurnOp>, open: &mut Option<Uuid>| match *open {
            Some(id) => id,
            None => {
                let (op, id) = begin(false, task);
                ops.push(op);
                *open = Some(id);
                id
            }
        };
        match *t {
            Transition::Started | Transition::Resumed => {}
            Transition::Prompted { prompt } => {
                if let Some(id) = open.take() {
                    ops.push(end(
                        id,
                        finished(now.last_reply.as_deref().unwrap_or_default(), now),
                    ));
                }
                let (op, id) = begin(true, Some(prompt));
                ops.push(op);
                open = Some(id);
            }
            Transition::Spoke { prompt } => match open {
                None if matches!(now.state, SessionState::Working | SessionState::Blocked) => {
                    let (op, id) = begin(true, Some(prompt));
                    ops.push(op);
                    open = Some(id);
                }
                _ => ops.push(settle()),
            },
            Transition::Blocked { pending } | Transition::ToTerminal { pending } => {
                let turn = working(&mut ops, &mut open);
                ops.push(TurnOp::Wait {
                    turn,
                    wait: wait_of(now.id, pending),
                });
            }
            Transition::Background { count, .. } => {
                if let Some(turn) = open {
                    ops.push(TurnOp::Background { turn, count, at });
                }
            }
            Transition::Unblocked { pending, .. } => {
                if let Some(turn) = open {
                    ops.push(TurnOp::Unwait {
                        turn,
                        key: pending.id.to_string(),
                        at,
                    });
                }
            }
            Transition::Finished { reply } => {
                let turn = working(&mut ops, &mut open);
                ops.push(end(turn, finished(reply, now)));
                open = None;
            }
            Transition::Asked { reply, asked } => {
                let turn = working(&mut ops, &mut open);
                ops.push(end(turn, asking_outcome(reply, asked)));
                open = None;
            }
            Transition::Halted => {
                if let Some(id) = open.take() {
                    ops.push(end(id, (Outcome::Stopped, None, None)));
                }
            }
            Transition::Dismissed => ops.push(settle()),
            Transition::Stopped { .. } | Transition::Interrupted => {
                let outcome = match t {
                    Transition::Interrupted => Outcome::Interrupted,
                    _ => Outcome::Stopped,
                };
                if let Some(id) = open.take() {
                    ops.push(end(id, (outcome, None, None)));
                }
                ops.push(settle());
            }
            Transition::Tool { tool } => {
                let turn = working(&mut ops, &mut open);
                ops.push(TurnOp::Progress {
                    turn,
                    doing: Some(format!("● {tool}")),
                    at,
                });
            }
        }
    }
    (ops, open)
}

/// How long a turn's task line is kept.
const TASK_CHARS: usize = 120;

/// How a turn that ended on `reply` without asking the person ended:
/// finished, or on its usage limit. A session in another terminal, whose
/// turns Desk reads from its transcript and not its hooks, is read for an
/// ask here, by the rules alone.
pub(super) fn finished(
    reply: &str,
    now: &SessionView,
) -> (Outcome, Option<String>, Option<ItemQuestion>) {
    match (asking::read(reply), crew::outcome_of(Ok(reply))) {
        (_, (Outcome::Limit, line, _)) => (Outcome::Limit, line, None),
        (Reading::Asks(asked), _) if now.state == SessionState::Idle => {
            asking_outcome(reply, &asked)
        }
        (_, (_, line, _)) => (Outcome::Finished, line, None),
    }
}

/// A turn that ended asking the person what `asked` says.
pub(super) fn asking_outcome(
    reply: &str,
    asked: &Asked,
) -> (Outcome, Option<String>, Option<ItemQuestion>) {
    (
        Outcome::Asked,
        crew::outcome_of(Ok(reply)).1,
        Some(ItemQuestion {
            text: asked.ask.clone(),
            options: asked.options.clone(),
        }),
    )
}

/// A pending request as the wait it makes.
fn wait_of(session: Uuid, p: &PendingApproval) -> Wait {
    Wait {
        key: p.id.to_string(),
        from: p.since,
        to: None,
        ask: crew::approval_summary(&p.tool, &p.detail),
        answer: match (p.tool.as_str(), p.answerable) {
            ("Input", _) => Answer::Reply,
            (_, true) => Answer::Decide {
                at: DecideRef::Session {
                    session_id: session,
                    approval_id: p.id,
                },
            },
            (_, false) => Answer::Terminal,
        },
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::model::{Approvals, Engine, Origin, SessionRecord};

    fn view(state: SessionState) -> SessionView {
        let now = Utc::now();
        SessionView::new(
            &SessionRecord {
                id: Uuid::nil(),
                engine: Engine::Claude,
                name: "auth-fix".into(),
                goal: String::new(),
                cwd: "/repo".into(),
                branch: None,
                origin: Origin::Desk,
                state,
                approvals: Approvals::Desk,
                cli_session_id: None,
                transcript: None,
                last_prompt: Some("fix the login".into()),
                last_reply: None,
                external_pid: None,
                restore_offer: false,
                terminal_id: None,
                display_name: None,
                asked: None,
                created_at: now,
                updated_at: now,
            },
            true,
            None,
            None,
        )
    }

    fn pending(tool: &str) -> PendingApproval {
        PendingApproval {
            id: Uuid::new_v4(),
            tool: tool.into(),
            detail: "cargo test".into(),
            since: Utc::now(),
            answerable: true,
        }
    }

    fn blocked(p: &PendingApproval) -> SessionView {
        SessionView {
            pending: Some(p.clone()),
            ..view(SessionState::Blocked)
        }
    }

    #[test]
    fn every_change_reads_as_what_it_means() {
        use SessionState as S;
        let none = HashSet::new();
        let working = view(S::Working);
        assert_eq!(Transition::of(None, &working, &none), [Transition::Started]);

        let done = SessionView {
            last_reply: Some("All green.".into()),
            ..view(S::Done)
        };
        assert_eq!(
            Transition::of(Some(&working), &done, &none),
            [Transition::Finished {
                reply: "All green."
            }]
        );
        let prompted = SessionView {
            last_prompt: Some("now the docs".into()),
            prompts: 1,
            ..working.clone()
        };
        assert_eq!(
            Transition::of(Some(&done), &prompted, &none),
            [Transition::Prompted {
                prompt: "now the docs"
            }]
        );
        assert_eq!(
            Transition::of(Some(&done), &working, &none),
            [],
            "no prompt, no new prompt"
        );
        let again = SessionView {
            prompts: 1,
            ..working.clone()
        };
        assert_eq!(
            Transition::of(Some(&done), &again, &none),
            [Transition::Prompted {
                prompt: "fix the login"
            }],
            "the same words said again are a new prompt"
        );
        let reread = SessionView {
            last_prompt: Some("the rollout's words".into()),
            ..done.clone()
        };
        assert_eq!(
            Transition::of(Some(&working), &reread, &none),
            [Transition::Finished {
                reply: "All green."
            }],
            "words read back as the turn ends are no prompt"
        );
        let said_as_it_ended = SessionView {
            prompts: 1,
            ..reread.clone()
        };
        assert_eq!(
            Transition::of(Some(&working), &said_as_it_ended, &none),
            [Transition::Finished {
                reply: "All green."
            }],
            "a turn that ends begins none"
        );

        let p = pending("Bash(cargo test)");
        let asks = blocked(&p);
        assert_eq!(
            Transition::of(Some(&working), &asks, &none),
            [Transition::Blocked { pending: &p }]
        );
        let q = pending("Edit(a.rs)");
        let asks_again = blocked(&q);
        assert_eq!(
            Transition::of(Some(&asks), &asks_again, &none),
            [
                Transition::Unblocked {
                    pending: &p,
                    by_desk: false
                },
                Transition::Blocked { pending: &q }
            ]
        );

        let tooling = SessionView {
            current_tool: Some("Bash(ls)".into()),
            ..working.clone()
        };
        assert_eq!(
            Transition::of(Some(&working), &tooling, &none),
            [Transition::Tool { tool: "Bash(ls)" }]
        );

        assert_eq!(
            Transition::of(Some(&working), &view(S::Stopped), &none),
            [Transition::Stopped { closed: false }]
        );
        assert_eq!(
            Transition::of(Some(&working), &view(S::Closed), &none),
            [Transition::Stopped { closed: true }]
        );
        assert_eq!(
            Transition::of(Some(&working), &view(S::Interrupted), &none),
            [Transition::Interrupted]
        );
        assert_eq!(
            Transition::of(Some(&view(S::Stopped)), &view(S::Starting), &none),
            [Transition::Resumed]
        );
    }

    #[test]
    fn a_request_cleared_with_nothing_after_it_is_answered() {
        let p = pending("Bash(cargo test)");
        let asks = blocked(&p);
        let working = view(SessionState::Working);
        assert_eq!(
            Transition::of(Some(&asks), &working, &HashSet::new()),
            [Transition::Unblocked {
                pending: &p,
                by_desk: false
            }],
            "answered in its terminal"
        );
        assert_eq!(
            Transition::of(Some(&asks), &working, &HashSet::from([p.id])),
            [Transition::Unblocked {
                pending: &p,
                by_desk: true
            }]
        );
        let done = view(SessionState::Done);
        assert_eq!(
            Transition::of(Some(&asks), &done, &HashSet::new()),
            [
                Transition::Unblocked {
                    pending: &p,
                    by_desk: false
                },
                Transition::Finished { reply: "" }
            ],
            "moved on past it"
        );
        assert_eq!(
            Transition::of(Some(&asks), &view(SessionState::Stopped), &HashSet::new()),
            [Transition::Stopped { closed: false }],
            "a session that went away answered nothing"
        );
    }

    #[test]
    fn activity_reads_each_change_as_before_and_terminal_answers_as_answered() {
        use super::super::{Desk, State};
        use crate::crew::Kind;
        let now = view(SessionState::Working);
        let p = pending("Bash(cargo test)");
        let asked = Asked {
            ask: "Ship it?".into(),
            why: "asked you a question".into(),
            options: None,
            denied: None,
        };
        let refused = Asked::refused(&[crate::asking::Denied::from_hook(
            "Bash",
            Some(&serde_json::json!({ "command": "git push --force" })),
            "[Git Destructive]",
        )])
        .unwrap();
        let cases: Vec<(Transition<'_>, Option<(Kind, &str)>)> = vec![
            (Transition::Started, Some((Kind::Started, "started"))),
            (
                Transition::Prompted {
                    prompt: "Fix the login. Then docs.",
                },
                Some((Kind::Started, "started “Fix the login.”")),
            ),
            (
                Transition::Blocked { pending: &p },
                Some((Kind::NeedsYou, "wants to run cargo test")),
            ),
            (
                Transition::Unblocked {
                    pending: &p,
                    by_desk: false,
                },
                Some((Kind::Answered, "was answered in its terminal")),
            ),
            (
                Transition::Unblocked {
                    pending: &p,
                    by_desk: true,
                },
                None,
            ),
            (
                Transition::Finished {
                    reply: "Fixed. 214 pass.",
                },
                Some((Kind::Finished, "finished · Fixed.")),
            ),
            (
                Transition::Finished { reply: "" },
                Some((Kind::Finished, "finished")),
            ),
            (
                Transition::Finished {
                    reply: "Done. Ship it?",
                },
                Some((Kind::Finished, "finished · Done.")),
            ),
            (
                Transition::Asked {
                    reply: "Done. Ship it?",
                    asked: &asked,
                },
                Some((Kind::Question, "asked “Ship it?”")),
            ),
            (
                Transition::Asked {
                    reply: "Blocked.",
                    asked: &refused,
                },
                Some((Kind::Question, "stopped: Bash was not allowed")),
            ),
            (Transition::Halted, None),
            (Transition::Dismissed, None),
            (
                Transition::Finished {
                    reply: "You've hit your usage limit.",
                },
                Some((Kind::Limit, "hit its usage limit")),
            ),
            (
                Transition::Stopped { closed: false },
                Some((Kind::Stopped, "stopped")),
            ),
            (
                Transition::Stopped { closed: true },
                Some((Kind::Stopped, "closed in its terminal")),
            ),
            (
                Transition::Interrupted,
                Some((Kind::Failed, "stopped unexpectedly")),
            ),
            (Transition::Resumed, Some((Kind::Resumed, "resumed"))),
            (
                Transition::Tool { tool: "Bash(ls)" },
                Some((Kind::Working, "is running Bash(ls)")),
            ),
        ];
        for (t, want) in cases {
            let mut st = State::default();
            let got = Desk::session_events(&mut st, &[t], &now);
            assert_eq!(
                got.iter()
                    .map(|e| (e.kind, e.text.as_str()))
                    .collect::<Vec<_>>(),
                want.into_iter().collect::<Vec<_>>(),
                "{t:?}"
            );
        }
        let mut st = State::default();
        st.acted.insert(now.id);
        assert!(
            Desk::session_events(&mut st, &[Transition::Stopped { closed: false }], &now)
                .is_empty(),
            "a stop someone's action recorded is not recorded again"
        );
    }

    fn ops(
        transitions: &[Transition<'_>],
        now: &SessionView,
        open: Option<Uuid>,
    ) -> (Vec<TurnOp>, Option<Uuid>) {
        let mut n = 0u128;
        turn_ops(transitions, now, open, now.updated_at, || {
            n += 1;
            Uuid::from_u128(n)
        })
    }

    fn kinds(ops: &[TurnOp]) -> Vec<&'static str> {
        ops.iter()
            .map(|op| match op {
                TurnOp::Begin {
                    by_person: true, ..
                } => "begin by person",
                TurnOp::Begin { .. } => "begin",
                TurnOp::Progress { .. } => "progress",
                TurnOp::Wait { .. } => "wait",
                TurnOp::Unwait { .. } => "unwait",
                TurnOp::End { .. } => "end",
                TurnOp::Background { .. } => "background",
                TurnOp::Settle { .. } => "settle",
            })
            .collect()
    }

    #[test]
    fn a_sessions_changes_write_its_turns() {
        use SessionState as S;
        let working = view(S::Working);
        let open = Some(Uuid::from_u128(99));
        let (o, after) = ops(&[Transition::Prompted { prompt: "fix it" }], &working, None);
        assert_eq!(
            (kinds(&o), after),
            (vec!["begin by person"], Some(Uuid::from_u128(1)))
        );
        let TurnOp::Begin { turn, .. } = &o[0] else {
            unreachable!()
        };
        assert_eq!(
            (turn.task.as_deref(), turn.owner.as_deref(), &turn.place),
            (Some("fix it"), None, &Place::Session)
        );

        let waited = SessionView {
            last_reply: Some("Launched it.".into()),
            ..working.clone()
        };
        let (o, after) = ops(&[Transition::Prompted { prompt: "again" }], &waited, open);
        assert_eq!(
            kinds(&o),
            ["end", "begin by person"],
            "a turn left open ends"
        );
        assert_eq!(after, Some(Uuid::from_u128(1)));
        let TurnOp::End {
            outcome, answer, ..
        } = &o[0]
        else {
            unreachable!()
        };
        assert_eq!(
            (*outcome, answer.as_deref()),
            (Outcome::Finished, Some("Launched it.")),
            "it had answered, and waited on its work"
        );

        let (o, after) = ops(&[Transition::Spoke { prompt: "and docs" }], &working, open);
        assert_eq!(
            (kinds(&o), after),
            (vec!["settle"], open),
            "spoken to while busy"
        );
        let (o, _) = ops(&[Transition::Spoke { prompt: "first" }], &working, None);
        assert_eq!(kinds(&o), ["begin by person"]);

        let p = pending("Bash(cargo test)");
        let (o, after) = ops(&[Transition::Blocked { pending: &p }], &working, None);
        assert_eq!(kinds(&o), ["begin", "wait"], "work seen without a prompt");
        assert_eq!(after, Some(Uuid::from_u128(1)));
        let TurnOp::Wait { wait, .. } = &o[1] else {
            unreachable!()
        };
        assert_eq!(
            (wait.key.clone(), wait.ask.as_str(), &wait.answer),
            (
                p.id.to_string(),
                "Run cargo test?",
                &Answer::Decide {
                    at: DecideRef::Session {
                        session_id: working.id,
                        approval_id: p.id
                    }
                }
            )
        );
        let terminal = PendingApproval {
            answerable: false,
            ..p.clone()
        };
        let (o, _) = ops(
            &[Transition::ToTerminal { pending: &terminal }],
            &working,
            open,
        );
        let TurnOp::Wait { wait, .. } = &o[0] else {
            unreachable!()
        };
        assert_eq!(wait.answer, Answer::Terminal);
        let input = PendingApproval {
            tool: "Input".into(),
            ..terminal
        };
        let (o, _) = ops(&[Transition::Blocked { pending: &input }], &working, open);
        let TurnOp::Wait { wait, .. } = &o[0] else {
            unreachable!()
        };
        assert_eq!(
            wait.answer,
            Answer::Reply,
            "asking in its terminal is a question"
        );

        let (o, after) = ops(
            &[Transition::Unblocked {
                pending: &p,
                by_desk: false,
            }],
            &working,
            open,
        );
        assert_eq!((kinds(&o), after), (vec!["unwait"], open));

        let (o, after) = ops(&[Transition::Tool { tool: "Bash(ls)" }], &working, open);
        assert_eq!((kinds(&o), after), (vec!["progress"], open));

        let (o, after) = ops(
            &[Transition::Finished {
                reply: "Done. Ship it?",
            }],
            &working,
            open,
        );
        assert_eq!((kinds(&o), after), (vec!["end"], None));
        let TurnOp::End {
            outcome, question, ..
        } = &o[0]
        else {
            unreachable!()
        };
        assert_eq!(
            (*outcome, question),
            (Outcome::Finished, &None),
            "a session's hooks read its turn's end: one that finished asks nothing"
        );

        let asked = Asked {
            ask: "Ship it?".into(),
            why: "asked you a question".into(),
            options: None,
            denied: None,
        };
        let (o, after) = ops(
            &[Transition::Asked {
                reply: "Done. Ship it?",
                asked: &asked,
            }],
            &working,
            open,
        );
        assert_eq!((kinds(&o), after), (vec!["end"], None));
        let TurnOp::End {
            outcome, question, ..
        } = &o[0]
        else {
            unreachable!()
        };
        assert_eq!(*outcome, Outcome::Asked);
        assert_eq!(question.as_ref().unwrap().text, "Ship it?");
        let (o, after) = ops(&[Transition::Halted], &working, open);
        assert_eq!((kinds(&o), after), (vec!["end"], None));
        let TurnOp::End { outcome, .. } = &o[0] else {
            unreachable!()
        };
        assert_eq!(*outcome, Outcome::Stopped, "interrupted, not finished");
        let (o, _) = ops(&[Transition::Dismissed], &working, None);
        assert_eq!(kinds(&o), ["settle"]);

        let (o, after) = ops(&[Transition::Stopped { closed: false }], &working, open);
        assert_eq!((kinds(&o), after), (vec!["end", "settle"], None));
        let (o, _) = ops(&[Transition::Interrupted], &working, None);
        assert_eq!(kinds(&o), ["settle"]);
        let (o, after) = ops(&[Transition::Started, Transition::Resumed], &working, open);
        assert_eq!((kinds(&o), after), (vec![], open));
    }

    #[test]
    fn another_terminals_session_ends_its_turn_as_it_goes_quiet() {
        use SessionState as S;
        let none = HashSet::new();
        let elsewhere = |state, prompt: &str| SessionView {
            live: false,
            origin: Origin::External,
            last_prompt: Some(prompt.into()),
            last_reply: Some("Done.".into()),
            ..view(state)
        };
        let busy = elsewhere(S::Working, "fix the login");
        let quiet = elsewhere(S::Idle, "fix the login");
        assert_eq!(
            Transition::of(Some(&busy), &quiet, &none),
            [Transition::Finished { reply: "Done." }]
        );
        assert_eq!(
            Transition::of(Some(&quiet), &elsewhere(S::Working, "and docs"), &none),
            [Transition::Prompted { prompt: "and docs" }],
            "its words are all there is to go by"
        );
        let whole_turn_unseen = elsewhere(S::Idle, "and docs");
        let seen = Transition::of(Some(&quiet), &whole_turn_unseen, &none);
        assert_eq!(seen, [Transition::Spoke { prompt: "and docs" }]);
        let (o, after) = ops(&seen, &whole_turn_unseen, None);
        assert_eq!(
            (kinds(&o), after),
            (vec!["settle"], None),
            "spoken to while quiet: it answered, and nothing runs"
        );
    }

    #[test]
    fn the_persons_prompt_reaching_it_otherwise_is_spoken() {
        use SessionState as S;
        let none = HashSet::new();
        let idle = view(S::Idle);
        let first = view(S::Working);
        assert_eq!(
            Transition::of(Some(&idle), &first, &none),
            [Transition::Spoke {
                prompt: "fix the login"
            }],
            "the prompt it was started with"
        );
        let typed = SessionView {
            last_prompt: Some("and the docs".into()),
            prompts: 1,
            ..first.clone()
        };
        assert_eq!(
            Transition::of(Some(&first), &typed, &none),
            [Transition::Spoke {
                prompt: "and the docs"
            }],
            "typed while it worked"
        );
        let p = pending("Bash(cargo test)");
        let asks = blocked(&p);
        let handed = blocked(&PendingApproval {
            answerable: false,
            ..p.clone()
        });
        assert_eq!(
            Transition::of(Some(&asks), &handed, &none),
            [Transition::ToTerminal {
                pending: handed.pending.as_ref().unwrap()
            }]
        );
    }

    #[test]
    fn a_turn_done_with_work_still_running_waits_on_it_until_it_ends() {
        use crate::floor::Background;
        use SessionState as S;
        let none = HashSet::new();
        let since = Utc::now();
        let with = |state, count: Option<u32>| SessionView {
            background: count.map(|count| Background { count, since }),
            last_reply: Some("Launched it.".into()),
            ..view(state)
        };
        let working = with(S::Working, Some(2));
        let waiting = with(S::Done, Some(2));
        assert_eq!(
            Transition::of(Some(&working), &waiting, &none),
            [Transition::Background { from: 0, count: 2 }],
            "not finished"
        );
        assert_eq!(
            Transition::of(Some(&waiting), &with(S::Done, Some(1)), &none),
            [Transition::Background { from: 2, count: 1 }]
        );
        assert_eq!(
            Transition::of(Some(&waiting), &with(S::Done, None), &none),
            [
                Transition::Background { from: 2, count: 0 },
                Transition::Finished {
                    reply: "Launched it."
                }
            ],
            "the last of it ended"
        );
        assert_eq!(
            Transition::of(Some(&waiting), &with(S::Working, Some(1)), &none),
            [Transition::Background { from: 2, count: 0 }],
            "it went on working"
        );
        assert_eq!(
            Transition::of(Some(&with(S::Working, None)), &with(S::Done, None), &none),
            [Transition::Finished {
                reply: "Launched it."
            }]
        );
        let (o, after) = ops(
            &[Transition::Background { from: 0, count: 2 }],
            &waiting,
            Some(Uuid::from_u128(99)),
        );
        assert_eq!(
            (kinds(&o), after),
            (vec!["background"], Some(Uuid::from_u128(99)))
        );
        let (o, after) = ops(
            &[Transition::Background { from: 0, count: 1 }],
            &waiting,
            None,
        );
        assert_eq!(
            (kinds(&o), after),
            (vec![], None),
            "work seen after its turn ended begins none"
        );
    }
}
