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
    /// When they were stopped, where Desk saw it.
    pub at: Option<DateTime<Utc>>,
}

/// The longest a stopped piece's name is said.
const NAMED_CHARS: usize = 120;
/// The most stopped pieces named; the rest are counted.
const NAMED_AT_MOST: usize = 8;

const NOTE_STARTS: &str = "This session was restarted at ";
const NOTE_ENDS: &str = " Check what each was doing, and restart any you still need.";

impl CutOff {
    /// `self` and `other`, both stopped and neither told yet: one.
    pub fn merged(mut self, other: CutOff) -> Self {
        for (kind, label) in other.named {
            if !self.named.iter().any(|(k, l)| *k == kind && *l == label) {
                self.named.push((kind, label));
            }
        }
        // Shells only seen running have no ids: counted all the same.
        let unnamed = other.count.saturating_sub(other.ids.len());
        let fresh = other.ids.difference(&self.ids).count();
        self.ids.extend(other.ids);
        // A count can be under its ids (shells reported, fewer seen).
        self.count += (fresh + unnamed).min(other.count);
        self.at = self.at.max(other.at);
        self
    }

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
            "{NOTE_STARTS}{at}. Background tasks and subagents started before \
             then were stopped.{which}{NOTE_ENDS}"
        )
    }

    /// `prompt` without the note Desk put before it: what was asked.
    /// `None` when the note is all there is.
    pub fn asked(prompt: &str) -> Option<&str> {
        let rest = match prompt.strip_prefix(NOTE_STARTS) {
            Some(_) => prompt
                .split_once(NOTE_ENDS)
                .map_or(prompt, |(_, rest)| rest)
                .trim_start(),
            None => prompt,
        };
        (!rest.is_empty()).then_some(rest)
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

/// How many looks in a row must find fewer shells running than the CLI
/// reported before those not found are taken to have ended: one look can
/// come before a shell's process starts.
pub const MISSED_LOOKS: u32 = 3;

/// Looks in a row that found fewer shells running than the CLI reported:
/// how many, and since when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Missed {
    looks: u32,
    first: DateTime<Utc>,
}

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
    /// The looks since the CLI's report that found fewer of its shells
    /// running than it reported.
    missed: Option<Missed>,
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
        let shells = match self.reported {
            true => reported,
            false => self.shells.unwrap_or(0),
        };
        agents + shells
    }

    /// The ids of the shells known running.
    pub fn shells(&self) -> impl Iterator<Item = &str> {
        self.tasks
            .iter()
            .filter(|(_, t)| t.kind == TaskKind::Shell)
            .map(|(id, _)| id.as_str())
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
            at: None,
        })
    }

    /// The tasks a turn's end reported running, or `None` from a CLI that
    /// reports none; what was heard of each is kept, and what an earlier
    /// look saw is not. After a restart, what was stopped with the last
    /// process is never counted, nor a sub-agent not heard starting since:
    /// a resumed CLI can report what it no longer runs.
    pub fn reported(mut self, tasks: Option<Vec<Reported>>, now: DateTime<Utc>) -> Self {
        self.shells = None;
        self.missed = None;
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
    /// sub-agent unheard past [`AGENT_QUIET`] with none running died, and
    /// the reported shells it didn't find ended once [`MISSED_LOOKS`] looks
    /// in a row over at least `unseen` missed them, and are never counted
    /// again. Those ended are the shells not `found` in a process of their
    /// own, where Desk's sampler says, then those heard of first.
    pub fn looked(
        mut self,
        shells: usize,
        found: Option<&BTreeSet<String>>,
        now: DateTime<Utc>,
        unseen: Duration,
    ) -> Self {
        let unfound = self.shells().count().saturating_sub(shells);
        let missed = (self.reported && unfound > 0).then(|| {
            let (looks, first) = self.missed.map_or((1, now), |m| (m.looks + 1, m.first));
            Missed { looks, first }
        });
        let gone = missed.is_some_and(|m| m.looks >= MISSED_LOOKS && now - m.first >= unseen);
        self.missed = missed.filter(|_| !gone);
        if gone {
            let mut ended: Vec<(bool, DateTime<Utc>, String)> = self
                .tasks
                .iter()
                .filter(|(_, t)| t.kind == TaskKind::Shell)
                .map(|(id, t)| (found.is_some_and(|f| f.contains(id)), t.heard, id.clone()))
                .collect();
            ended.sort();
            ended.into_iter().take(unfound).for_each(|(.., id)| {
                self.tasks.remove(&id);
            });
        }
        self.shells = Some(shells);
        if shells == 0 {
            self.tasks
                .retain(|_, t| t.kind == TaskKind::Shell || now - t.heard < AGENT_QUIET);
        }
        self.dated(now)
    }

    /// [`Self::looked`], when there was a look.
    pub fn looked_if(
        self,
        shells: Option<usize>,
        found: Option<&BTreeSet<String>>,
        now: DateTime<Utc>,
        unseen: Duration,
    ) -> Self {
        match shells {
            Some(shells) => self.looked(shells, found, now, unseen),
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

    const UNSEEN: Duration = Duration::minutes(2);

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
        assert_eq!(CutOff::asked(&note), None);
        assert_eq!(
            CutOff::asked(&format!("{note}\n\nCarry on")),
            Some("Carry on")
        );
        assert_eq!(CutOff::asked("Carry on"), Some("Carry on"));
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
        // Stopped twice before it was told: each piece once, the shells
        // only seen running too.
        let named = CutOff {
            ids: ["a1".to_string()].into(),
            named: vec![(TaskKind::Agent, "Watch CI".into())],
            count: 1,
            ..CutOff::default()
        };
        let both = named.clone().merged(unnamed).merged(named);
        assert_eq!((both.count, both.named.len()), (4, 1));
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
        assert_eq!(b.clone().looked(1, None, at(1), UNSEEN).count(), 1);
        let unreported = Background::default().reported(None, at(0));
        assert!(
            unreported.worth_a_look(),
            "a CLI that reports nothing is looked at"
        );
        assert_eq!(
            unreported.looked(2, None, at(1), UNSEEN).count(),
            2,
            "its shells are counted by process"
        );
    }

    #[test]
    fn a_reported_shell_is_kept_until_looks_in_a_row_miss_it_past_the_grace() {
        let b = Background::default().reported(tasks(&[("s1", TaskKind::Shell)]), at(0));
        let missed = b.clone().looked(0, None, at(0), UNSEEN);
        assert_eq!(missed.count(), 1, "one look can come before it starts");
        assert!(missed.worth_a_look() && missed.view().is_some());
        assert_eq!(missed.shells().collect::<Vec<_>>(), ["s1"]);
        let missed = missed
            .looked(0, None, at(1), UNSEEN)
            .looked(0, None, at(1), UNSEEN);
        assert_eq!(
            missed.count(),
            1,
            "missed three times, but within the grace"
        );
        let found = missed.clone().looked(1, None, at(1), UNSEEN);
        assert_eq!(found.count(), 1);
        let after = found
            .looked(0, None, at(2), UNSEEN)
            .looked(0, None, at(3), UNSEEN);
        assert_eq!(
            after.clone().looked(0, None, at(3), UNSEEN).count(),
            1,
            "found again, its misses count anew"
        );
        let gone = after.looked(0, None, at(4), UNSEEN);
        assert_eq!(
            (gone.count(), gone.view()),
            (0, None),
            "missed long enough: it ended"
        );
        assert_eq!(gone.shells().count(), 0);
        assert!(!gone.worth_a_look());
        let quick = b
            .clone()
            .looked(0, None, at(0), UNSEEN)
            .looked(0, None, at(3), UNSEEN);
        assert_eq!(quick.count(), 1, "two looks are never enough");
        let reported_again = missed
            .reported(tasks(&[("s1", TaskKind::Shell)]), at(1))
            .looked(0, None, at(5), UNSEEN);
        assert_eq!(
            reported_again.count(),
            1,
            "the CLI says it runs: looked at anew"
        );
        let ended = b.reported(tasks(&[]), at(1));
        assert_eq!(ended.count(), 0, "the CLI's own report ends it at once");
    }

    #[test]
    fn a_shell_written_off_beside_one_that_runs_is_the_one_not_found_and_never_counted_again() {
        let b = Background::default()
            .reported(tasks(&[("s1", TaskKind::Shell)]), at(0))
            .reported(
                tasks(&[("s1", TaskKind::Shell), ("s2", TaskKind::Shell)]),
                at(1),
            );
        let found = BTreeSet::from(["s1".to_string()]);
        let looks = |b: Background, found: Option<&BTreeSet<String>>| {
            let missed = b
                .looked(1, found, at(1), UNSEEN)
                .looked(1, found, at(2), UNSEEN);
            assert_eq!(missed.count(), 2, "within the grace");
            missed.looked(1, found, at(3), UNSEEN)
        };
        let gone = looks(b.clone(), Some(&found));
        assert_eq!(gone.count(), 1, "missed long enough: one ended");
        assert_eq!(
            gone.shells().collect::<Vec<_>>(),
            ["s1"],
            "the later one, found in no process, while the earlier runs on"
        );
        let call = gone.clone().looked(2, Some(&found), at(4), UNSEEN);
        assert_eq!(
            call.count(),
            1,
            "a foreground call's shell never brings it back"
        );
        assert_eq!(call.looked(1, Some(&found), at(5), UNSEEN).count(), 1);
        assert_eq!(
            gone.looked(0, Some(&found), at(9), UNSEEN).count(),
            1,
            "one look again"
        );
        assert_eq!(
            looks(b, None).shells().collect::<Vec<_>>(),
            ["s2"],
            "with nothing found, the one heard of first"
        );
    }

    #[test]
    fn its_shells_are_the_running_pieces_that_are_shells() {
        let b = Background::default().reported(
            tasks(&[("s1", TaskKind::Shell), ("a1", TaskKind::Agent)]),
            at(0),
        );
        assert_eq!(b.shells().collect::<Vec<_>>(), ["s1"]);
        assert_eq!(b.ended("s1", at(1)).shells().count(), 0);
    }

    #[test]
    fn a_turns_report_is_newer_than_the_last_look() {
        let b = Background::default()
            .reported(tasks(&[("s1", TaskKind::Shell)]), at(0))
            .looked(0, None, at(1), UNSEEN)
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
            b.clone().looked(0, None, at(9), UNSEEN).count(),
            1,
            "still within its quiet"
        );
        assert_eq!(
            b.clone().looked(1, None, at(30), UNSEEN).count(),
            1,
            "a shell of its still runs"
        );
        let heard = b.clone().heard("a1", at(8));
        assert_eq!(
            heard.clone().looked(0, None, at(15), UNSEEN).count(),
            1,
            "heard from lately"
        );
        assert_eq!(
            heard.looked(0, None, at(19), UNSEEN).count(),
            0,
            "it died without a stop"
        );
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
                .looked(0, None, at(15), UNSEEN)
                .count(),
            1,
            "heard working lately"
        );
        assert_eq!(b.ended("t2", at(2)).view(), None);
    }
}
