//! Work a session started that runs on after its turn: Claude Code's
//! background shells and sub-agents, Codex's sub-agents and background
//! terminals. How each report, and each look at the session's processes,
//! changes what is known of it.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use crate::floor;

/// What a piece of background work is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Shell,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Task {
    kind: TaskKind,
    /// When it was last heard from.
    heard: DateTime<Utc>,
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

    /// The tasks a turn's end reported running, or `None` from a CLI that
    /// reports none; what was heard of each is kept, and what an earlier
    /// look saw is not.
    pub fn reported(mut self, tasks: Option<Vec<(String, TaskKind)>>, now: DateTime<Utc>) -> Self {
        self.shells = None;
        match tasks {
            Some(tasks) => {
                let heard = std::mem::take(&mut self.tasks);
                self.tasks = tasks
                    .into_iter()
                    .map(|(id, kind)| {
                        let heard = heard.get(&id).map_or(now, |t| t.heard);
                        (id, Task { kind, heard })
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
            .or_insert(Task { kind, heard: now });
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

    fn tasks(list: &[(&str, TaskKind)]) -> Option<Vec<(String, TaskKind)>> {
        Some(list.iter().map(|(id, k)| (id.to_string(), *k)).collect())
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
        assert_eq!(b.count(), 1, "the next turn's shell runs until a look says not");
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
