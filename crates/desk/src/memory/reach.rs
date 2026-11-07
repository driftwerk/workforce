//! Who may read what of session memory. Pure: every fact comes from the
//! server (the caller's token, the registry, the sidebar), never from what
//! the caller asked, which can only narrow it.
//!
//! A group is the client boundary, whether or not it has a boss: a session
//! reads only its own group's collection. An overseer reads what its scope
//! sees ([`Seen`]): a bossed group's memory is never the main overseer's,
//! a group gone from the sidebar is no overseer's, and of a group that
//! stopped being a boundary the main overseer reads only what was written
//! after. The person reads everything.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::passage::collection_of;
use crate::scope::Seen;

/// Who is reading.
#[derive(Debug, Clone, PartialEq)]
pub enum Reader {
    /// The person, in Desk itself.
    Person,
    /// An overseer, as its scope sees things now.
    Overseer(Seen),
    /// A session, from its line token and the registry's record of it.
    Session {
        id: Uuid,
        /// Its group; none when ungrouped.
        group: Option<String>,
        project: String,
        /// The sessions of its lineage (its work), itself among them.
        work: Vec<Uuid>,
    },
}

impl Reader {
    /// How `memory_reads.reader` names it.
    pub fn label(&self) -> String {
        match self {
            Reader::Person => "person".into(),
            Reader::Overseer(seen) => match seen.scope.group() {
                Some(g) => format!("overseer:{g}"),
                None => "overseer:main".into(),
            },
            Reader::Session { id, .. } => format!("session:{id}"),
        }
    }
}

/// How far a session's recall goes within its group, narrowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reach {
    /// Its own work: its lineage's sessions.
    Work,
    /// Its project.
    Project,
    /// Its whole group.
    Group,
}

impl Reach {
    pub fn parse(s: &str) -> Option<Reach> {
        match s {
            "work" => Some(Reach::Work),
            "project" => Some(Reach::Project),
            "group" => Some(Reach::Group),
            _ => None,
        }
    }
}

/// What a reader may search.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Bound {
    /// The groups whose rows it may read (`None`: ungrouped).
    pub groups: Vec<Option<String>>,
    /// Their collections, in the same order.
    pub collections: Vec<String>,
    /// Only these sessions' rows, when set.
    pub sessions: Option<Vec<Uuid>>,
    /// Only these projects' rows, when set.
    pub projects: Option<Vec<String>>,
}

impl Bound {
    fn of(groups: Vec<Option<String>>) -> Self {
        Self {
            collections: groups.iter().map(|g| collection_of(g.as_deref())).collect(),
            groups,
            ..Self::default()
        }
    }

    /// This bound kept to one `group` (`Some(None)`: ungrouped) and one
    /// `session` where asked: what is asked can only take away.
    pub fn narrowed(mut self, group: Option<Option<&str>>, session: Option<Uuid>) -> Self {
        if let Some(g) = group {
            let keep: Vec<Option<String>> = self
                .groups
                .into_iter()
                .filter(|have| have.as_deref() == g)
                .collect();
            self = Bound {
                sessions: self.sessions,
                projects: self.projects,
                ..Bound::of(keep)
            };
        }
        if let Some(s) = session {
            self.sessions = Some(match self.sessions {
                Some(have) => have.into_iter().filter(|h| *h == s).collect(),
                None => vec![s],
            });
        }
        self
    }

    /// Whether nothing at all may be read.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() || self.sessions.as_ref().is_some_and(Vec::is_empty)
    }
}

/// What `reader` may search: of the `known` groups (those session memory
/// holds rows of), for a session within the narrower of the install's
/// `setting` and what it `asked`.
pub fn bound(
    reader: &Reader,
    setting: Reach,
    asked: Option<Reach>,
    known: &[Option<String>],
) -> Bound {
    let mut known: Vec<Option<String>> = known.to_vec();
    known.sort();
    known.dedup();
    match reader {
        Reader::Person => Bound::of(known),
        Reader::Overseer(seen) => Bound::of(
            known
                .into_iter()
                .filter(|g| seen.sees(g.as_deref()))
                .collect(),
        ),
        Reader::Session {
            id,
            group,
            project,
            work,
        } => {
            // Its own group's collection, whether or not it has rows yet.
            let mut b = Bound::of(vec![group.clone()]);
            match asked.map_or(setting, |a| a.min(setting)) {
                Reach::Work => {
                    let mut sessions: Vec<Uuid> = work.clone();
                    sessions.push(*id);
                    sessions.sort();
                    sessions.dedup();
                    b.sessions = Some(sessions);
                }
                Reach::Project => b.projects = Some(vec![project.clone()]),
                Reach::Group => {}
            }
            b
        }
    }
}

/// A row as stored, read back from Postgres before any of its text is
/// returned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Row<'a> {
    pub group: Option<&'a str>,
    pub session: Uuid,
    pub project: &'a str,
    pub at: DateTime<Utc>,
}

/// The last check on each row, against its stored group, before any text
/// of it is returned: within `bound`, and one `reader` may read whatever
/// the index or a filter said.
pub fn may_see(reader: &Reader, bound: &Bound, row: &Row) -> bool {
    let in_bound = bound.groups.iter().any(|g| g.as_deref() == row.group)
        && bound
            .sessions
            .as_ref()
            .is_none_or(|s| s.contains(&row.session))
        && bound
            .projects
            .as_ref()
            .is_none_or(|p| p.iter().any(|p| p == row.project));
    in_bound
        && match reader {
            Reader::Person => true,
            Reader::Overseer(seen) => seen.sees_row(row.group, row.at),
            Reader::Session { group, .. } => group.as_deref() == row.group,
        }
}

/// The groups among `rows` (for `memory_reads.group_id`): one when every
/// row is of the same.
pub fn one_group<'a>(groups: impl Iterator<Item = Option<&'a str>>) -> Option<String> {
    let set: HashSet<Option<&str>> = groups.collect();
    match set.len() {
        1 => set.into_iter().next().flatten().map(str::to_string),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::Scope;

    const ACME: &str = "g-acme";
    const GLOBEX: &str = "g-globex";
    const GONE: &str = "g-gone";

    fn known() -> Vec<Option<String>> {
        vec![
            None,
            Some(ACME.into()),
            Some(GLOBEX.into()),
            Some(GONE.into()),
        ]
    }

    /// Acme with its boss when `bossed`, else open; Globex open; Gone no
    /// longer in the sidebar.
    fn seen(scope: Scope, bossed: bool) -> Seen {
        let (b, mut open): (HashSet<String>, HashSet<String>) = match bossed {
            true => ([ACME.to_string()].into(), HashSet::new()),
            false => (HashSet::new(), [ACME.to_string()].into()),
        };
        open.insert(GLOBEX.into());
        Seen::new(scope, b, open)
    }

    fn session(group: Option<&str>) -> Reader {
        Reader::Session {
            id: Uuid::from_u128(1),
            group: group.map(str::to_string),
            project: "/dev/acme-web".into(),
            work: vec![Uuid::from_u128(2)],
        }
    }

    fn row(group: Option<&str>) -> Row<'_> {
        Row {
            group,
            session: Uuid::from_u128(1),
            project: "/dev/acme-web",
            at: Utc::now(),
        }
    }

    /// Whether `reader` sees a row of `group`, by its bound and the last
    /// check alike; the two must agree.
    fn sees(reader: &Reader, group: Option<&str>) -> bool {
        let b = bound(reader, Reach::Group, None, &known());
        let by_bound = b.groups.iter().any(|g| g.as_deref() == group)
            && b.collections.contains(&collection_of(group));
        let by_row = may_see(reader, &b, &row(group));
        assert_eq!(by_bound, by_row, "{reader:?} on {group:?}");
        by_row
    }

    #[test]
    fn every_reader_sees_exactly_its_groups_rows_bossed_or_not() {
        let groups: [Option<&str>; 4] = [None, Some(ACME), Some(GLOBEX), Some(GONE)];
        // Each reader: what it sees of ungrouped, Acme, Globex and Gone.
        for bossed in [true, false] {
            let cases: Vec<(&str, Reader, [bool; 4])> = vec![
                ("the person", Reader::Person, [true, true, true, true]),
                (
                    "the main overseer",
                    Reader::Overseer(seen(Scope::Ungrouped, bossed)),
                    [true, !bossed, true, false],
                ),
                (
                    "Acme's boss",
                    Reader::Overseer(seen(Scope::Group(ACME.into()), bossed)),
                    // Acme without a boss is no scope of its own.
                    [false, bossed, false, false],
                ),
                (
                    "an overseer Desk can't place",
                    Reader::Overseer(Seen::nothing()),
                    [false, false, false, false],
                ),
                (
                    "an Acme session",
                    session(Some(ACME)),
                    [false, true, false, false],
                ),
                (
                    "a Globex session",
                    session(Some(GLOBEX)),
                    [false, false, true, false],
                ),
                (
                    "an ungrouped session",
                    session(None),
                    [true, false, false, false],
                ),
            ];
            for (who, reader, want) in cases {
                let got = groups.map(|g| sees(&reader, g));
                assert_eq!(got, want, "{who}, Acme bossed: {bossed}");
            }
        }
    }

    #[test]
    fn the_main_overseer_reads_a_former_boundarys_rows_only_from_when_it_opened() {
        let mut s = seen(Scope::Ungrouped, false);
        let opened = Utc::now();
        s.opened.insert(ACME.into(), opened);
        let reader = Reader::Overseer(s);
        let b = bound(&reader, Reach::Group, None, &known());
        let before = Row {
            at: opened - chrono::Duration::minutes(1),
            ..row(Some(ACME))
        };
        let after = Row {
            at: opened + chrono::Duration::minutes(1),
            ..row(Some(ACME))
        };
        assert!(!may_see(&reader, &b, &before));
        assert!(may_see(&reader, &b, &after));
    }

    #[test]
    fn a_sessions_reach_is_the_narrower_of_the_setting_and_what_it_asked() {
        let reader = session(Some(ACME));
        let b = |setting, asked| bound(&reader, setting, asked, &known());
        assert_eq!(
            b(Reach::Project, None).projects,
            Some(vec!["/dev/acme-web".to_string()])
        );
        // Asking wider than the setting gets the setting.
        assert_eq!(
            b(Reach::Project, Some(Reach::Group)),
            b(Reach::Project, None)
        );
        assert_eq!(b(Reach::Work, Some(Reach::Group)), b(Reach::Work, None));
        // Asking narrower narrows.
        let work = b(Reach::Group, Some(Reach::Work));
        assert_eq!(
            work.sessions,
            Some(vec![Uuid::from_u128(1), Uuid::from_u128(2)])
        );
        assert_eq!(work.projects, None);
        let group = b(Reach::Group, None);
        assert_eq!((group.sessions, group.projects), (None, None));
        // Whatever it asks, only its own group's collection.
        for setting in [Reach::Work, Reach::Project, Reach::Group] {
            for asked in [
                None,
                Some(Reach::Work),
                Some(Reach::Project),
                Some(Reach::Group),
            ] {
                let got = b(setting, asked);
                assert_eq!(got.collections, [collection_of(Some(ACME))]);
                assert!(
                    got.sessions.is_some() || got.projects.is_some() || setting == Reach::Group
                );
            }
        }
        // Another project's row, within the group, is out of a project reach.
        let p = b(Reach::Project, None);
        let elsewhere = Row {
            project: "/dev/acme-api",
            ..row(Some(ACME))
        };
        assert!(!may_see(&reader, &p, &elsewhere));
        assert!(may_see(&reader, &b(Reach::Group, None), &elsewhere));
        let other = Row {
            session: Uuid::from_u128(9),
            ..row(Some(ACME))
        };
        assert!(!may_see(&reader, &work, &other), "outside its work");
    }

    #[test]
    fn a_filter_only_takes_away() {
        let all = bound(&Reader::Person, Reach::Group, None, &known());
        let acme = all.clone().narrowed(Some(Some(ACME)), None);
        assert_eq!(acme.groups, [Some(ACME.to_string())]);
        assert_eq!(acme.collections, [collection_of(Some(ACME))]);
        let ungrouped = all.clone().narrowed(Some(None), None);
        assert_eq!(ungrouped.groups, [None]);
        // A session reader asking for another group gets nothing.
        let s = bound(&session(Some(ACME)), Reach::Group, None, &known());
        let globex = s.narrowed(Some(Some(GLOBEX)), None);
        assert!(globex.is_empty());
        let one = all.narrowed(None, Some(Uuid::from_u128(7)));
        assert_eq!(one.sessions, Some(vec![Uuid::from_u128(7)]));
        let work = bound(&session(Some(ACME)), Reach::Work, None, &known());
        assert!(
            work.narrowed(None, Some(Uuid::from_u128(9))).is_empty(),
            "outside its work"
        );
    }

    #[test]
    fn readers_are_named_as_memory_reads_keeps_them() {
        assert_eq!(Reader::Person.label(), "person");
        assert_eq!(
            Reader::Overseer(seen(Scope::Ungrouped, true)).label(),
            "overseer:main"
        );
        assert_eq!(
            Reader::Overseer(seen(Scope::Group(ACME.into()), true)).label(),
            "overseer:g-acme"
        );
        assert_eq!(
            session(None).label(),
            format!("session:{}", Uuid::from_u128(1))
        );
        assert_eq!(
            one_group([Some(ACME), Some(ACME)].into_iter()),
            Some(ACME.into())
        );
        assert_eq!(one_group([Some(ACME), None].into_iter()), None);
    }
}
