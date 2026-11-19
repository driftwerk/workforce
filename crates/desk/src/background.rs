//! Work a session started that runs on after its turn: Claude Code's
//! background shells and sub-agents, Codex's sub-agents and background
//! terminals. How each report, and each look at the session's processes,
//! changes what is known of it.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};

use crate::floor;

/// What a piece of background work is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Shell,
    Agent,
}

/// A piece a turn's end reported running: its id, what it is, and what
/// the CLI calls it (a sub-agent's description, a shell's command), when
/// it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    pub id: String,
    pub kind: TaskKind,
    pub label: Option<String>,
}

impl Reported {
    #[cfg(test)]
    pub fn new(id: &str, kind: TaskKind) -> Self {
        Self {
            id: id.to_string(),
            kind,
            label: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Task {
    kind: TaskKind,
    /// When it was last heard from.
    heard: DateTime<Utc>,
    /// What the CLI calls it, when it said.
    label: Option<String>,
}

/// What a session had running when its process ended: stopped with it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CutOff {
    /// The ids the CLI knew them by: never counted again once it runs anew.
    pub ids: BTreeSet<String>,
    /// Those Desk knows what they were: a sub-agent's description, a
    /// shell's command.
    pub named: Vec<(TaskKind, String)>,
    /// How many were running, as far as Desk knew.
    pub count: usize,
}

/// The longest a stopped piece's name is said.
const NAMED_CHARS: usize = 120;
/// The most stopped pieces named; the rest are counted.
const NAMED_AT_MOST: usize = 8;

impl CutOff {
    /// What the session is told first when it runs again, restarted at
    /// `at` (local time, "18:32"): what was stopped, and to look at it.
    pub fn note(&self, at: &str) -> String {
        let mut named: Vec<String> = self
            .named
            .iter()
            .take(NAMED_AT_MOST)
            .map(|(kind, label)| {
                let label = clipped(label);
                match kind {
                    TaskKind::Agent => format!("the subagent “{label}”"),
                    TaskKind::Shell => format!("the shell `{label}`"),
                }
            })
            .collect();
        let more = self.count.saturating_sub(named.len());
        if more > 0 && !named.is_empty() {
            named.push(match more {
                1 => "1 more".into(),
                n => format!("{n} more"),
            });
        }
        let which = match named.is_empty() {
            true => String::new(),
            false => format!(" Desk knew of {}.", named.join(", ")),
        };
        format!(
            "This session was restarted at {at}. Background tasks and subagents started before \
             then were stopped.{which} Check what each was doing, and restart any you still need."
        )
    }
}

/// `label` on one line, at most [`NAMED_CHARS`] long.
fn clipped(label: &str) -> String {
    let line = label.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.chars().count() > NAMED_CHARS {
        true => format!("{}…", line.chars().take(NAMED_CHARS).collect::<String>()),
        false => line,
    }
}

/// How long a sub-agent may go unheard, with no shell of its session
/// running, before it is taken to have died without saying so.
pub const AGENT_QUIET: Duration = Duration::minutes(10);

/// What a session has running in the background.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Background {
    /// What the CLI reported, by its id.
    tasks: BTreeMap<String, Task>,
    /// Whether the CLI reports its shells; when it doesn't, they are counted
    /// by process.
    reported: bool,
    /// Shells seen running under the session at the last look.
    shells: Option<usize>,
    /// Since when the session has had some.
    since: Option<DateTime<Utc>>,
    /// Run again after a restart: what its last process had running,
    /// stopped with it. What a resumed CLI still reports of those is never
    /// counted, and a sub-agent counts only once heard starting here.
    restarted: Option<BTreeSet<String>>,
}

impl Background {
    /// How many pieces are running.
    pub fn count(&self) -> usize {
        let agents = self
            .tasks
            .values()
            .filter(|t| t.kind == TaskKind::Agent)
            .count();
        let reported = self.tasks.len() - agents;
        let shells = match (self.reported, self.shells) {
            (true, Some(seen)) => reported.min(seen),
            (true, None) => reported,
            (false, seen) => seen.unwrap_or(0),
        };
        agents + shells
    }

    /// As the floor shows it, while there is any.
    pub fn view(&self) -> Option<floor::Background> {
        let count = u32::try_from(self.count()).ok().filter(|c| *c > 0)?;
        Some(floor::Background {
            count,
            since: self.since?,
        })
    }

    /// Keeps `since` true to the count.
    fn dated(mut self, now: DateTime<Utc>) -> Self {
        self.since = match self.count() {
            0 => None,
            _ => self.since.or(Some(now)),
        };
        self
    }

    /// What a session runs with after a restart that stopped `cut` (or
    /// none known): nothing yet, and nothing of what was stopped.
    pub fn after_restart(cut: Option<&CutOff>) -> Self {
        Self {
            restarted: Some(cut.map(|c| c.ids.clone()).unwrap_or_default()),
            ..Self::default()
        }
    }

    /// What it has running now, as stopped if its process ends now; none
    /// while nothing is.
    pub fn cut_off(&self) -> Option<CutOff> {
        let count = self.count();
        (count > 0).then(|| CutOff {
            ids: self.tasks.keys().cloned().collect(),
            named: self
                .tasks
                .values()
                .filter_map(|t| Some((t.kind, t.label.clone()?)))
                .filter(|(_, l)| !l.trim().is_empty())
                .collect(),
            count,
        })
    }

    /// The tasks a turn's end reported running, or `None` from a CLI that
    /// reports none; what was heard of each is kept, and what an earlier
    /// look saw is not. After a restart, what was stopped with the last
    /// process is never counted, nor a sub-agent not heard starting since:
    /// a resumed CLI can report what it no longer runs.
    pub fn reported(mut self, tasks: Option<Vec<Reported>>, now: DateTime<Utc>) -> Self {
        self.shells = None;
        match tasks {
            Some(tasks) => {
                let heard = std::mem::take(&mut self.tasks);
                let restarted = self.restarted.as_ref();
                self.tasks = tasks
                    .into_iter()
                    .filter(|t| {
                        restarted.is_none_or(|gone| {
                            !gone.contains(&t.id)
                                && (t.kind == TaskKind::Shell || heard.contains_key(&t.id))
                        })
                    })
                    .map(|t| {
                        let was = heard.get(&t.id);
                        let task = Task {
                            kind: t.kind,
                            heard: was.map_or(now, |w| w.heard),
                            label: t.label.or_else(|| was.and_then(|w| w.label.clone())),
                        };
                        (t.id, task)
                    })
                    .collect();
                self.reported = true;
            }
            None => {
                self.tasks.clear();
                self.reported = false;
            }
        }
        self.dated(now)
    }

    /// A sub-agent was heard from.
    pub fn heard(mut self, id: &str, now: DateTime<Utc>) -> Self {
        if let Some(t) = self.tasks.get_mut(id) {
            t.heard = now;
        }
        self
    }

    /// Whether the piece `id` (a sub-agent, a shell) runs, as last known.
    pub fn runs(&self, id: &str) -> bool {
        self.tasks.contains_key(id)
    }

    /// One piece ended.
    pub fn ended(mut self, id: &str, now: DateTime<Utc>) -> Self {
        self.tasks.remove(id);
        self.dated(now)
    }

    /// A piece the CLI's own events track started, or was heard working.
    pub fn started(mut self, id: &str, kind: TaskKind, now: DateTime<Utc>) -> Self {
        self.tasks
            .entry(id.to_string())
            .and_modify(|t| t.heard = now)
            .or_insert(Task {
                kind,
                heard: now,
                label: None,
            });
        self.dated(now)
    }

    /// A look at the session's processes found `shells` running: a
    /// sub-agent unheard past [`AGENT_QUIET`] with none running died.
    pub fn looked(mut self, shells: usize, now: DateTime<Utc>) -> Self {
        self.shells = Some(shells);
        if shells == 0 {
            self.tasks
                .retain(|_, t| t.kind != TaskKind::Agent || now - t.heard < AGENT_QUIET);
        }
        self.dated(now)
    }

    /// [`Self::looked`], when there was a look.
    pub fn looked_if(self, shells: Option<usize>, now: DateTime<Utc>) -> Self {
        match shells {
            Some(shells) => self.looked(shells, now),
            None => self,
        }
    }

    /// Whether a look at the processes could change it.
    pub fn worth_a_look(&self) -> bool {
        self.count() > 0 || !self.reported
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-29T16:00:00Z")
            .unwrap()
            .to_utc()
            + Duration::minutes(minutes)
    }

    fn tasks(list: &[(&str, TaskKind)]) -> Option<Vec<Reported>> {
        Some(list.iter().map(|(id, k)| Reported::new(id, *k)).collect())
    }

    #[test]
    fn what_a_restart_stopped_is_never_counted_again() {
        let mut named = tasks(&[("a1", TaskKind::Agent), ("s1", TaskKind::Shell)]).unwrap();
        named[0].label = Some("Watch CI on #450".into());
        named[1].label = Some("gh run watch 123".into());
        let before = Background::default().reported(Some(named), at(0));
        let cut = before.cut_off().expect("it had some running");
        assert_eq!(cut.count, 2);
        let note = cut.note("18:32");
        assert!(
            note.starts_with("This session was restarted at 18:32."),
            "{note}"
        );
        assert!(note.contains("the subagent “Watch CI on #450”"), "{note}");
        assert!(note.contains("the shell `gh run watch 123`"), "{note}");
        assert!(note.ends_with("restart any you still need."), "{note}");
        // Run again: the resumed CLI still reports them, and a sub-agent it
        // never started here: none of it counts.
        let after = Background::after_restart(Some(&cut)).reported(
            tasks(&[
                ("a1", TaskKind::Agent),
                ("s1", TaskKind::Shell),
                ("a9", TaskKind::Agent),
            ]),
            at(1),
        );
        assert_eq!(after.view(), None, "nothing it runs now");
        // What it starts now counts as ever.
        let now = Background::after_restart(Some(&cut))
            .started("a2", TaskKind::Agent, at(2))
            .reported(
                tasks(&[("a2", TaskKind::Agent), ("s2", TaskKind::Shell)]),
                at(3),
            );
        assert_eq!(now.count(), 2);
        assert_eq!(
            Background::default().cut_off(),
            None,
            "nothing running, nothing stopped"
        );
        let unnamed = CutOff {
            count: 3,
            ..CutOff::default()
        };
        assert!(!unnamed.note("09:05").contains("Desk knew of"));
    }

    #[test]
    fn what_a_turn_left_running_is_counted_until_it_ends() {
        let b = Background::default().reported(
            tasks(&[("a1", TaskKind::Agent), ("s1", TaskKind::Shell)]),
            at(0),
        );
        assert_eq!(
            b.view(),
            Some(floor::Background {
                count: 2,
                since: at(0)
            })
        );
        let b = b.ended("a1", at(5));
        assert_eq!(b.view().map(|v| (v.count, v.since)), Some((1, at(0))));
        let b = b.reported(tasks(&[]), at(6));
        assert_eq!(b.view(), None);
        assert!(!b.worth_a_look(), "nothing left, and the CLI says so");
    }

    #[test]
    fn a_shell_counts_while_its_process_runs() {
        let b = Background::default().reported(tasks(&[("s1", TaskKind::Shell)]), at(0));
        assert_eq!(b.clone().looked(1, at(1)).count(), 1);
        assert_eq!(
            b.clone().looked(0, at(1)).count(),
            0,
            "it ended without a word"
        );
        let unreported = Background::default().reported(None, at(0));
        assert!(
            unreported.worth_a_look(),
            "a CLI that reports nothing is looked at"
        );
        assert_eq!(
            unreported.looked(2, at(1)).count(),
            2,
            "its shells are counted by process"
        );
    }

    #[test]
    fn a_turns_report_is_newer_than_the_last_look() {
        let b = Background::default()
            .reported(tasks(&[("s1", TaskKind::Shell)]), at(0))
            .looked(0, at(1))
            .reported(tasks(&[("s2", TaskKind::Shell)]), at(2));
        assert_eq!(
            b.count(),
            1,
            "the next turn's shell runs until a look says not"
        );
    }

    #[test]
    fn a_sub_agent_unheard_with_nothing_running_has_died() {
        let b = Background::default().reported(tasks(&[("a1", TaskKind::Agent)]), at(0));
        assert_eq!(
            b.clone().looked(0, at(9)).count(),
            1,
            "still within its quiet"
        );
        assert_eq!(
            b.clone().looked(1, at(30)).count(),
            1,
            "a shell of its still runs"
        );
        let heard = b.clone().heard("a1", at(8));
        assert_eq!(
            heard.clone().looked(0, at(15)).count(),
            1,
            "heard from lately"
        );
        assert_eq!(heard.looked(0, at(19)).count(), 0, "it died without a stop");
    }

    #[test]
    fn a_sub_agent_the_cli_tracks_starts_and_ends() {
        let b = Background::default()
            .started("t2", TaskKind::Agent, at(0))
            .started("t2", TaskKind::Agent, at(1));
        assert_eq!(b.view().map(|v| (v.count, v.since)), Some((1, at(0))));
        assert_eq!(
            b.clone()
                .started("t2", TaskKind::Agent, at(9))
                .looked(0, at(15))
                .count(),
            1,
            "heard working lately"
        );
        assert_eq!(b.ended("t2", at(2)).view(), None);
    }
}
