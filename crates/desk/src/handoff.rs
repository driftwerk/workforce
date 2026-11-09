//! Continue in another engine: what a handoff is asked, and whether it may
//! be made. Pure; [`crate::service`]'s handoff reads the facts from the
//! registry, the sidebar and session memory, never from what was asked,
//! and makes it.
//!
//! A handoff never crosses groups: the session it starts is in the
//! source's group, and a session handed back to must still be in it. It
//! never starts or leaves a session on a machine's engine until groups are
//! granted to machines, and never hands on the hub's own runs.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::engine::{self, Engine, View};

/// Where the session that continues the work runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// The source's own folder.
    #[default]
    SameFolder,
    /// A new worktree, on a new branch from the source's HEAD.
    NewWorktree,
}

/// What becomes of the session the work leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum After {
    /// Stopped now: the default when it is idle.
    Stop,
    /// Stopped once its turn ends.
    AfterTurn,
    /// Left running: two engines then edit one tree.
    Keep,
}

impl After {
    /// What happens to a source that says nothing: stopped once it is idle.
    pub fn default_for(working: bool) -> After {
        match working {
            true => After::AfterTurn,
            false => After::Stop,
        }
    }
}

/// What the person asks of a handoff.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ask {
    pub engine: Option<Engine>,
    /// The account it runs on: absent, the engine's default; `null`, the
    /// CLI's own sign-in.
    #[serde(default, deserialize_with = "crate::model::present")]
    pub account: Option<Option<Uuid>>,
    #[serde(default)]
    pub place: Place,
    /// What becomes of the source; by default stopped once it is idle.
    pub source: Option<After>,
    /// Hand back: resume the session this work came from, with what
    /// happened since, instead of starting one.
    #[serde(default)]
    pub back: bool,
    /// Start with only the goal, when the source's conversation can't be
    /// handed over.
    #[serde(default)]
    pub goal_only: bool,
    /// The person confirmed the brief goes to the target's vendor, where
    /// the source's group asks each time.
    #[serde(default)]
    pub egress_confirmed: bool,
}

/// The facts a handoff is checked against, all read on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// This hub may be a machine's ([`crate::memory::MemoryOptions::machine`]).
    pub machine: bool,
    pub source_engine: Engine,
    /// The source is one of the hub's own runs.
    pub hub_internal: bool,
    /// The source's own Remember switch.
    pub memory_on: bool,
    /// Desk can read the source's conversation from its engine's record.
    pub record: bool,
    /// Desk can hide secrets in what it keeps, here.
    pub redactor: bool,
    /// The source's group now; none ungrouped.
    pub source_group: Option<String>,
    /// The work already continues in another session.
    pub handed_to: Option<Uuid>,
    /// The target's engine.
    pub target: Engine,
    /// The target's group: the source's for a new session, the resumed
    /// session's own for a hand back.
    pub target_group: Option<String>,
    /// The target's account is signed in (or it runs on the CLI's own
    /// sign-in, which is).
    pub signed_in: bool,
    /// For a hand back: the session it resumes can be resumed by id, isn't
    /// running, and is the one this work came from.
    pub back: Option<Back>,
    /// The source runs in a Desk terminal: only the person ends it.
    pub in_terminal: bool,
}

/// What a hand back resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Back {
    pub engine: Engine,
    pub resumable: bool,
    pub running: bool,
}

/// Why a handoff can't be made, in the person's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Machine,
    NotStartable(Engine),
    NotSignedIn(Engine),
    CrossGroup,
    HubInternal,
    MemoryOff(Engine),
    NoRecord { source: Engine, target: Engine },
    NoRedactor,
    AlreadyHanded,
    NothingToHandBack,
    CantResume,
    StillRunning,
    BackToAnother(Engine),
    InTerminal,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Machine => f.write_str(crate::memory::MACHINE_HANDOFF),
            Refusal::NotStartable(e) => write!(f, "Desk can't start {} sessions here yet.", e.title()),
            Refusal::NotSignedIn(e) => write!(f, "That {} account isn't signed in; sign it in again, or choose another.", e.title()),
            Refusal::CrossGroup => f.write_str("Work can't be handed to a session in another group."),
            Refusal::HubInternal => f.write_str("This is one of the hub's own runs, so it can't be handed on."),
            Refusal::MemoryOff(e) => write!(f, "Desk isn't remembering this session, so it has nothing to hand over. Turn Remember on, or start {} here with only the goal.", e.title()),
            Refusal::NoRecord { source, target } => write!(f, "Desk can't read {} conversations, so it can't hand this one over. Start {} here with only the goal instead.", source.title(), target.title()),
            Refusal::NoRedactor => f.write_str(crate::memory::NO_REDACTOR),
            Refusal::AlreadyHanded => f.write_str("This work already continues in another session; continue it from there."),
            Refusal::NothingToHandBack => f.write_str("This session wasn't handed work, so there is nothing to hand back."),
            Refusal::CantResume => f.write_str("The session this work came from can't be resumed; continue it in a new session instead."),
            Refusal::StillRunning => f.write_str("The session this work came from is still running; stop it first, or continue in a new session."),
            Refusal::BackToAnother(e) => write!(f, "This work came from a {} session; hand it back to that engine, or continue in a new session.", e.title()),
            Refusal::InTerminal => f.write_str("This session runs in a Desk terminal, so Desk can't stop it: keep it running, or end it there first."),
        }
    }
}

/// What a handoff hands over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Content {
    /// The brief: goal, state, notes and exchanges.
    Brief,
    /// Only the goal: the conversation can't be handed over.
    GoalOnly,
}

/// Whether a handoff may be made, and what it hands over.
pub fn check(f: &Facts, ask: &Ask) -> Result<Content, Refusal> {
    if f.machine {
        return Err(Refusal::Machine);
    }
    if f.hub_internal {
        return Err(Refusal::HubInternal);
    }
    if !engine::runs(f.target, View::Terminal) {
        return Err(Refusal::NotStartable(f.target));
    }
    if f.source_group != f.target_group {
        return Err(Refusal::CrossGroup);
    }
    if f.handed_to.is_some() {
        return Err(Refusal::AlreadyHanded);
    }
    match (ask.back, f.back) {
        (true, None) => return Err(Refusal::NothingToHandBack),
        (true, Some(b)) => {
            if b.engine != f.target {
                return Err(Refusal::BackToAnother(b.engine));
            }
            if !b.resumable {
                return Err(Refusal::CantResume);
            }
            if b.running {
                return Err(Refusal::StillRunning);
            }
        }
        (false, _) => {
            if !f.signed_in {
                return Err(Refusal::NotSignedIn(f.target));
            }
        }
    }
    if f.in_terminal && ask.source.unwrap_or(After::Keep) != After::Keep {
        return Err(Refusal::InTerminal);
    }
    if ask.goal_only {
        return Ok(Content::GoalOnly);
    }
    if !f.record {
        return Err(Refusal::NoRecord {
            source: f.source_engine,
            target: f.target,
        });
    }
    if !f.memory_on {
        return Err(Refusal::MemoryOff(f.target));
    }
    if !f.redactor {
        return Err(Refusal::NoRedactor);
    }
    Ok(Content::Brief)
}

/// The words that say where the brief goes: "The brief goes to OpenAI,
/// through Codex."
pub fn egress(target: Engine) -> String {
    match engine::vendor_of(target) {
        Some(v) => format!("The brief goes to {v}, through {}.", target.title()),
        None => format!("The brief goes to whatever {} sends it to.", target.title()),
    }
}

/// How a session came to hold its work: handed it from another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    /// The work's first session.
    pub work: Uuid,
    /// The session it came from.
    pub from: Uuid,
    pub at: DateTime<Utc>,
    /// The engine it was handed to.
    pub engine: Engine,
    /// The group it was handed over in; none ungrouped.
    pub group: Option<String>,
    /// What its brief held.
    pub included: crate::memory::brief::Included,
}

/// Which sessions are one piece of work, as kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lineage {
    /// Each session handed work, and how.
    pub links: HashMap<Uuid, Link>,
    /// Each session whose work continues in another, and which.
    pub handed_to: HashMap<Uuid, Uuid>,
}

impl Lineage {
    /// The work `session` is part of: its first session's id, else its own.
    pub fn work_of(&self, session: Uuid) -> Uuid {
        self.links.get(&session).map_or(session, |l| l.work)
    }

    /// Every session of `session`'s work, the first first, then as they
    /// were handed work.
    pub fn sessions(&self, session: Uuid) -> Vec<Uuid> {
        let work = self.work_of(session);
        let mut rest: Vec<(&Uuid, &Link)> = self
            .links
            .iter()
            .filter(|(s, l)| l.work == work && **s != work)
            .collect();
        rest.sort_by_key(|(s, l)| (l.at, **s));
        std::iter::once(work)
            .chain(rest.into_iter().map(|(s, _)| *s))
            .collect()
    }

    /// Where `session`'s work goes on now: followed along each hand on, to
    /// the session that holds it.
    pub fn head(&self, session: Uuid) -> Uuid {
        let mut at = session;
        let mut seen = vec![at];
        while let Some(next) = self.handed_to.get(&at).copied() {
            if seen.contains(&next) {
                break;
            }
            seen.push(next);
            at = next;
        }
        at
    }

    /// Whether `session`'s work went on in another.
    pub fn handed_off(&self, session: Uuid) -> bool {
        self.handed_to.contains_key(&session)
    }

    /// Every session `session`'s work was handed on to, and on from those,
    /// in the order they were handed it: whose gists may carry its
    /// purpose.
    pub fn carried_from(&self, session: Uuid) -> Vec<Uuid> {
        let mut reached = vec![session];
        let mut by: Vec<(&Uuid, &Link)> = self.links.iter().collect();
        by.sort_by_key(|(s, l)| (l.at, **s));
        loop {
            let more: Vec<Uuid> = by
                .iter()
                .filter(|(s, l)| reached.contains(&l.from) && !reached.contains(s))
                .map(|(s, _)| **s)
                .collect();
            if more.is_empty() {
                break;
            }
            reached.extend(more);
        }
        reached.remove(0);
        reached
    }

    /// `session` was forgotten: it holds no work, and the sessions whose
    /// work went on in it are where it is again.
    pub fn forget(&mut self, session: Uuid) {
        self.links.remove(&session);
        self.handed_to.remove(&session);
        self.handed_to.retain(|_, to| *to != session);
    }

    /// Notes that `to` was handed `from`'s work with `link`, and, when it
    /// was handed back, that `to`'s goes on no further.
    pub fn hand_on(&mut self, from: Uuid, to: Uuid, link: Link) {
        self.links.insert(to, link);
        self.handed_to.insert(from, to);
        self.handed_to.remove(&to);
    }
}

/// The name a handoff's new worktree's branch takes: the source's branch
/// (or its folder's name) and the target engine.
pub fn branch_for(source_branch: Option<&str>, folder: &str, target: Engine) -> String {
    let base = source_branch
        .map(str::to_string)
        .filter(|b| !b.is_empty() && b != "HEAD")
        .unwrap_or_else(|| {
            std::path::Path::new(folder)
                .file_name()
                .map_or_else(|| "work".into(), |n| n.to_string_lossy().into_owned())
        });
    format!("{base}-{}", target.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            machine: false,
            source_engine: Engine::Claude,
            hub_internal: false,
            memory_on: true,
            record: true,
            redactor: true,
            source_group: Some("g-acme".into()),
            handed_to: None,
            target: Engine::Codex,
            target_group: Some("g-acme".into()),
            signed_in: true,
            back: None,
            in_terminal: false,
        }
    }

    #[test]
    fn a_handoff_in_one_group_on_this_computer_hands_over_the_brief() {
        assert_eq!(check(&facts(), &Ask::default()), Ok(Content::Brief));
        let same_engine = Facts {
            target: Engine::Claude,
            ..facts()
        };
        assert_eq!(check(&same_engine, &Ask::default()), Ok(Content::Brief));
    }

    #[test]
    fn each_refusal_says_why() {
        let ask = Ask::default();
        let cases: Vec<(Facts, Refusal)> = vec![
            (
                Facts {
                    machine: true,
                    ..facts()
                },
                Refusal::Machine,
            ),
            (
                Facts {
                    hub_internal: true,
                    ..facts()
                },
                Refusal::HubInternal,
            ),
            (
                Facts {
                    target: Engine::Devin,
                    ..facts()
                },
                Refusal::NotStartable(Engine::Devin),
            ),
            (
                Facts {
                    target_group: Some("g-globex".into()),
                    ..facts()
                },
                Refusal::CrossGroup,
            ),
            (
                Facts {
                    target_group: None,
                    ..facts()
                },
                Refusal::CrossGroup,
            ),
            (
                Facts {
                    source_group: None,
                    ..facts()
                },
                Refusal::CrossGroup,
            ),
            (
                Facts {
                    signed_in: false,
                    ..facts()
                },
                Refusal::NotSignedIn(Engine::Codex),
            ),
            (
                Facts {
                    record: false,
                    source_engine: Engine::Custom,
                    ..facts()
                },
                Refusal::NoRecord {
                    source: Engine::Custom,
                    target: Engine::Codex,
                },
            ),
            (
                Facts {
                    memory_on: false,
                    ..facts()
                },
                Refusal::MemoryOff(Engine::Codex),
            ),
            (
                Facts {
                    redactor: false,
                    ..facts()
                },
                Refusal::NoRedactor,
            ),
            (
                Facts {
                    handed_to: Some(Uuid::nil()),
                    ..facts()
                },
                Refusal::AlreadyHanded,
            ),
        ];
        for (f, why) in cases {
            assert_eq!(check(&f, &ask), Err(why.clone()), "{why}");
            assert!(!why.to_string().is_empty());
        }
        assert_eq!(
            Refusal::Machine.to_string(),
            "Continuing on a machine comes with group grants."
        );
    }

    #[test]
    fn the_machine_refusal_comes_first_whatever_else_is_asked() {
        let f = Facts {
            machine: true,
            target_group: Some("g-globex".into()),
            ..facts()
        };
        for ask in [
            Ask::default(),
            Ask {
                goal_only: true,
                ..Ask::default()
            },
            Ask {
                back: true,
                ..Ask::default()
            },
        ] {
            assert_eq!(check(&f, &ask), Err(Refusal::Machine));
        }
    }

    #[test]
    fn only_the_goal_goes_when_the_conversation_cant() {
        let ask = Ask {
            goal_only: true,
            ..Ask::default()
        };
        for f in [
            Facts {
                record: false,
                ..facts()
            },
            Facts {
                memory_on: false,
                ..facts()
            },
            Facts {
                redactor: false,
                ..facts()
            },
        ] {
            assert_eq!(check(&f, &ask), Ok(Content::GoalOnly));
        }
        // Never across groups, even with only the goal.
        let f = Facts {
            target_group: None,
            ..facts()
        };
        assert_eq!(check(&f, &ask), Err(Refusal::CrossGroup));
    }

    #[test]
    fn a_hand_back_resumes_only_the_session_it_came_from_when_it_can() {
        let ask = Ask {
            back: true,
            ..Ask::default()
        };
        let back = |engine, resumable, running| Back {
            engine,
            resumable,
            running,
        };
        assert_eq!(check(&facts(), &ask), Err(Refusal::NothingToHandBack));
        let ok = Facts {
            back: Some(back(Engine::Codex, true, false)),
            // The work continues here: a hand back is how it goes back.
            handed_to: None,
            ..facts()
        };
        assert_eq!(check(&ok, &ask), Ok(Content::Brief));
        for (b, why) in [
            (
                back(Engine::Claude, true, false),
                Refusal::BackToAnother(Engine::Claude),
            ),
            (back(Engine::Codex, false, false), Refusal::CantResume),
            (back(Engine::Codex, true, true), Refusal::StillRunning),
        ] {
            assert_eq!(
                check(
                    &Facts {
                        back: Some(b),
                        ..facts()
                    },
                    &ask
                ),
                Err(why)
            );
        }
        let moved = Facts {
            back: Some(back(Engine::Codex, true, false)),
            target_group: Some("g-globex".into()),
            ..facts()
        };
        assert_eq!(
            check(&moved, &ask),
            Err(Refusal::CrossGroup),
            "it moved to another client"
        );
    }

    #[test]
    fn a_terminal_session_can_only_be_kept_running() {
        let f = Facts {
            in_terminal: true,
            ..facts()
        };
        let stop = Ask {
            source: Some(After::Stop),
            ..Ask::default()
        };
        assert_eq!(check(&f, &stop), Err(Refusal::InTerminal));
        let keep = Ask {
            source: Some(After::Keep),
            ..Ask::default()
        };
        assert_eq!(check(&f, &keep), Ok(Content::Brief));
    }

    #[test]
    fn the_egress_line_names_the_vendor() {
        assert_eq!(
            egress(Engine::Codex),
            "The brief goes to OpenAI, through Codex."
        );
        assert_eq!(
            egress(Engine::Claude),
            "The brief goes to Anthropic, through Claude Code."
        );
        assert!(egress(Engine::Custom).contains("whatever"));
    }

    fn link(work: Uuid, from: Uuid, min: i64) -> Link {
        Link {
            work,
            from,
            at: DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::minutes(min),
            engine: Engine::Codex,
            group: None,
            included: Default::default(),
        }
    }

    #[test]
    fn a_work_is_its_first_session_and_every_one_handed_it() {
        let (a, b, c, other) = (
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            Uuid::from_u128(3),
            Uuid::from_u128(9),
        );
        let mut l = Lineage::default();
        l.hand_on(a, b, link(a, a, 1));
        assert_eq!(l.work_of(b), a);
        assert_eq!(l.work_of(a), a);
        assert_eq!(l.head(a), b);
        assert!(l.handed_off(a) && !l.handed_off(b));
        // Handed back: one lineage, and its head is the first again.
        l.hand_on(b, a, link(a, b, 2));
        assert_eq!(l.head(a), a);
        assert_eq!(l.head(b), a);
        assert!(!l.handed_off(a) && l.handed_off(b));
        l.hand_on(a, c, link(a, a, 3));
        assert_eq!(l.sessions(c), vec![a, b, c]);
        assert_eq!(l.sessions(other), vec![other]);
        assert_eq!(l.head(b), c);
        assert_eq!(l.carried_from(a), vec![b, c], "all the way down");
        assert_eq!(l.carried_from(b), vec![a, c], "handed back to, and on");
        assert!(l.carried_from(other).is_empty());
        // Its last session forgotten, the work is where it was before.
        l.forget(c);
        assert_eq!(l.head(a), a);
        assert!(!l.handed_off(a));
        assert_eq!(l.sessions(a), vec![a, b]);
    }

    #[test]
    fn a_new_worktree_branches_after_the_source() {
        assert_eq!(
            branch_for(Some("fix/race"), "/src/acme", Engine::Codex),
            "fix/race-codex"
        );
        assert_eq!(
            branch_for(None, "/src/acme-api", Engine::Claude),
            "acme-api-claude"
        );
        assert_eq!(
            branch_for(Some("HEAD"), "/src/acme-api", Engine::Claude),
            "acme-api-claude"
        );
    }
}
