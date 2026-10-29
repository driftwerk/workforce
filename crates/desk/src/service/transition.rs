//! What one change of a session means: the single reading of a session's
//! before and after that its activity is recorded from.

use std::collections::HashSet;

use uuid::Uuid;

use crate::model::{PendingApproval, SessionState, SessionView};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Transition<'a> {
    /// First seen.
    Started,
    /// The person's prompt set it working again.
    Prompted {
        prompt: &'a str,
    },
    Blocked {
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
                if matches!(before.state, S::Done | S::Idle | S::Starting)
                    && now.last_prompt != before.last_prompt =>
            {
                now.last_prompt
                    .as_deref()
                    .map(|prompt| Self::Prompted { prompt })
            }
            S::Done if before.state != S::Done => Some(Self::Finished {
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
        let tool = now
            .current_tool
            .as_deref()
            .filter(|_| now.state == S::Working && now.current_tool != before.current_tool)
            .map(|tool| Self::Tool { tool });
        unblocked.into_iter().chain(state).chain(tool).collect()
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
            "the same prompt again is no new prompt"
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
                Some((Kind::Question, "asked “Ship it?”")),
            ),
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
}
