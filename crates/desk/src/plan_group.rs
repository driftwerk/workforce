//! The group a plan's work belongs to when nothing names one, read from its
//! tickets: each votes for at most one group, by the first of the session
//! it takes on, the sessions working in its folder, and a group named in
//! its folder's or repository's path below the home folder. The group with
//! the most votes wins; a tie or no vote picks none.

use std::collections::BTreeMap;

/// One ticket of a plan, as its group is read from it.
#[derive(Debug, Clone, Copy)]
pub struct Work<'a> {
    pub folder: &'a str,
    pub repo: Option<&'a str>,
    /// The group of the session it takes on, when that session is in one.
    pub session: Option<&'a str>,
}

/// One of this computer's open sessions in a group.
#[derive(Debug, Clone, Copy)]
pub struct Working<'a> {
    pub cwd: &'a str,
    pub group: &'a str,
}

/// A group the plan's boss may put it in.
#[derive(Debug, Clone, Copy)]
pub struct Choice<'a> {
    pub id: &'a str,
    pub name: &'a str,
}

/// Why a group was picked: for the first ticket that voted for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// A session it takes on is in the group.
    Session,
    /// The group's sessions work in this folder.
    Folder(String),
    /// Its path names the group.
    Named(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    pub group: String,
    pub why: Why,
}

/// The group `work` belongs to among `groups`, by `working`'s sessions;
/// `home` is the person's home folder.
pub fn pick(work: &[Work], working: &[Working], groups: &[Choice], home: &str) -> Option<Picked> {
    let votes = work
        .iter()
        .filter_map(|w| vote(w, working, groups, home))
        .fold(
            BTreeMap::<&str, (usize, Why)>::new(),
            |mut votes, (g, why)| {
                votes.entry(g).and_modify(|v| v.0 += 1).or_insert((1, why));
                votes
            },
        );
    most(votes, |(n, _)| *n).map(|(group, (_, why))| Picked {
        group: group.to_string(),
        why,
    })
}

/// The one entry of `counts` with the highest count; none for a tie.
fn most<K, V>(counts: BTreeMap<K, V>, count: impl Fn(&V) -> usize) -> Option<(K, V)> {
    let top = counts.values().map(&count).max()?;
    let mut at_top = counts.into_iter().filter(|(_, v)| count(v) == top);
    match (at_top.next(), at_top.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

fn vote<'a>(
    w: &Work,
    working: &[Working],
    groups: &[Choice<'a>],
    home: &str,
) -> Option<(&'a str, Why)> {
    let known = |id: &str| groups.iter().find(|g| g.id == id).map(|g| g.id);
    w.session
        .and_then(known)
        .map(|g| (g, Why::Session))
        .or_else(|| {
            by_folder(w.folder, working, groups).map(|g| (g, Why::Folder(w.folder.to_string())))
        })
        .or_else(|| named(w, groups, home))
}

/// The one group of `groups` most of the sessions working in `folder` (or
/// a worktree or subfolder of it) are in.
fn by_folder<'a>(folder: &str, working: &[Working], groups: &[Choice<'a>]) -> Option<&'a str> {
    let counts = working
        .iter()
        .filter(|s| within(folder, s.cwd))
        .filter_map(|s| groups.iter().find(|g| g.id == s.group))
        .fold(BTreeMap::<&str, usize>::new(), |mut n, g| {
            *n.entry(g.id).or_default() += 1;
            n
        });
    most(counts, |n| *n).map(|(g, _)| g)
}

const SEPARATORS: [char; 2] = ['/', '\\'];

/// Whether `cwd` is `folder` or under it.
fn within(folder: &str, cwd: &str) -> bool {
    below(folder, cwd).is_some()
}

/// What of `path` is under `folder` (empty for `folder` itself), when
/// `path` is `folder` or under it.
fn below<'p>(folder: &str, path: &'p str) -> Option<&'p str> {
    let folder = folder.trim_end_matches(SEPARATORS);
    let path = path.trim_end_matches(SEPARATORS);
    path.strip_prefix(folder)
        .filter(|rest| !folder.is_empty() && (rest.is_empty() || rest.starts_with(SEPARATORS)))
}

/// The group a segment of `w`'s folder (below `home`, when it is under it)
/// or repository names, the deepest segment first.
fn named<'a>(w: &Work, groups: &[Choice<'a>], home: &str) -> Option<(&'a str, Why)> {
    deepest(below(home, w.folder).unwrap_or(w.folder))
        .chain(w.repo.into_iter().flat_map(deepest))
        .find_map(|seg| {
            let folded = fold(seg);
            groups
                .iter()
                .find(|g| !folded.is_empty() && fold(g.name) == folded)
                .map(|g| (g.id, Why::Named(seg.to_string())))
        })
}

/// `path`'s segments, the deepest first, without a `.git` ending.
fn deepest(path: &str) -> impl Iterator<Item = &str> {
    path.rsplit(SEPARATORS)
        .map(|s| s.strip_suffix(".git").unwrap_or(s))
        .filter(|s| !s.is_empty())
}

fn fold(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROUPS: [Choice; 3] = [
        Choice {
            id: "g-mantis",
            name: "Mantis",
        },
        Choice {
            id: "g-atlas",
            name: "Atlas",
        },
        Choice {
            id: "g-blank",
            name: "··",
        },
    ];

    fn work(folder: &str) -> Work<'_> {
        Work {
            folder,
            repo: None,
            session: None,
        }
    }

    const HOME: &str = "/Users/sam";

    fn picked(work: &[Work], working: &[Working]) -> Option<(String, Why)> {
        pick(work, working, &GROUPS, HOME).map(|p| (p.group, p.why))
    }

    #[test]
    fn a_tickets_session_then_its_folder_then_its_path_names_its_group() {
        let working = [Working {
            cwd: "/w/mantis/monorepo/.worktrees/man-1",
            group: "g-atlas",
        }];
        let by_session = Work {
            session: Some("g-mantis"),
            ..work("/w/mantis/monorepo")
        };
        assert_eq!(
            picked(&[by_session], &working),
            Some(("g-mantis".into(), Why::Session))
        );
        assert_eq!(
            picked(&[work("/w/mantis/monorepo")], &working),
            Some(("g-atlas".into(), Why::Folder("/w/mantis/monorepo".into())))
        );
        assert_eq!(
            picked(&[work("/w/mantis/monorepo")], &[]),
            Some(("g-mantis".into(), Why::Named("mantis".into())))
        );
        let by_repo = Work {
            repo: Some("github.com/acme/Atlas.git"),
            ..work("")
        };
        assert_eq!(
            picked(&[by_repo], &[]),
            Some(("g-atlas".into(), Why::Named("Atlas".into())))
        );
        let unknown_session = Work {
            session: Some("g-gone"),
            ..work("/w/atlas")
        };
        assert_eq!(
            picked(&[unknown_session], &[]),
            Some(("g-atlas".into(), Why::Named("atlas".into())))
        );
    }

    #[test]
    fn the_most_votes_win_and_a_tie_or_nothing_picks_no_group() {
        let two = [work("/w/mantis/a"), work("/w/mantis/b"), work("/w/atlas")];
        assert_eq!(picked(&two, &[]).map(|(g, _)| g), Some("g-mantis".into()));
        assert_eq!(picked(&[work("/w/mantis"), work("/w/atlas")], &[]), None);
        assert_eq!(picked(&[work("/w/other")], &[]), None);
        assert_eq!(picked(&[], &[]), None);
        let split = [
            Working {
                cwd: "/w/x",
                group: "g-mantis",
            },
            Working {
                cwd: "/w/x",
                group: "g-atlas",
            },
        ];
        assert_eq!(picked(&[work("/w/x")], &split), None);
    }

    #[test]
    fn a_folder_matches_its_worktrees_and_subfolders_but_not_a_sibling_with_the_same_prefix() {
        assert!(within("/w/mantis", "/w/mantis"));
        assert!(within("/w/mantis/", "/w/mantis/.worktrees/man-2"));
        assert!(within("/w/mantis", "/w/mantis/sub/"));
        assert!(!within("/w/mantis", "/w/mantis-old"));
        assert!(!within("/w/mantis", "/w"));
        assert!(!within("", "/w"));
        assert!(within(r"C:\w\mantis", r"C:\w\mantis\.worktrees\man-2"));
        assert!(!within(r"C:\w\mantis", r"C:\w\mantis-old"));
    }

    #[test]
    fn the_home_folder_and_what_is_above_it_name_no_group() {
        let groups = [
            Choice {
                id: "g-sam",
                name: "Sam",
            },
            Choice {
                id: "g-users",
                name: "Users",
            },
        ];
        let at = |folder| pick(&[work(folder)], &[], &groups, HOME).map(|p| p.group);
        assert_eq!(at("/Users/sam/code/acme"), None);
        assert_eq!(at("/Users/sam"), None);
        assert_eq!(at("/Users/sam/sam/acme"), Some("g-sam".into()));
        assert_eq!(
            at("/srv/users/acme"),
            Some("g-users".into()),
            "outside home, every segment counts"
        );
    }

    #[test]
    fn a_group_the_boss_doesnt_see_or_an_empty_name_never_wins() {
        let only_atlas = [GROUPS[1]];
        assert_eq!(pick(&[work("/w/mantis")], &[], &only_atlas, HOME), None);
        let working = [
            Working {
                cwd: "/w/mantis",
                group: "g-mantis",
            },
            Working {
                cwd: "/w/mantis",
                group: "g-mantis",
            },
            Working {
                cwd: "/w/mantis",
                group: "g-atlas",
            },
        ];
        assert_eq!(
            pick(&[work("/w/mantis")], &working, &only_atlas, HOME).map(|p| p.group),
            Some("g-atlas".into()),
            "the group it sees, however many it doesn't see work there"
        );
        assert_eq!(picked(&[work("/w/··/x")], &[]), None);
    }
}
