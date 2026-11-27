//! The group a plan's work belongs to when nothing names one, read from its
//! tickets: each votes for at most one group, by the first of the session
//! it takes on, the sessions working in its folder, and a group named in
//! its folder's or repository's path. The group with the most votes wins;
//! a tie or no vote picks none.

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

/// The group `work` belongs to among `groups`, by `working`'s sessions.
pub fn pick(work: &[Work], working: &[Working], groups: &[Choice]) -> Option<Picked> {
    let votes = work.iter().filter_map(|w| vote(w, working, groups)).fold(
        BTreeMap::<&str, (usize, Why)>::new(),
        |mut votes, (g, why)| {
            votes.entry(g).and_modify(|v| v.0 += 1).or_insert((1, why));
            votes
        },
    );
    let most = votes.values().map(|(n, _)| *n).max()?;
    let mut top = votes.into_iter().filter(|(_, (n, _))| *n == most);
    match (top.next(), top.next()) {
        (Some((group, (_, why))), None) => Some(Picked {
            group: group.to_string(),
            why,
        }),
        _ => None,
    }
}

fn vote<'a>(w: &Work, working: &[Working], groups: &[Choice<'a>]) -> Option<(&'a str, Why)> {
    let known = |id: &str| groups.iter().find(|g| g.id == id).map(|g| g.id);
    w.session
        .and_then(known)
        .map(|g| (g, Why::Session))
        .or_else(|| {
            by_folder(w.folder, working, groups).map(|g| (g, Why::Folder(w.folder.to_string())))
        })
        .or_else(|| named(w, groups))
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
    let most = counts.values().copied().max()?;
    let mut top = counts.into_iter().filter(|(_, n)| *n == most);
    match (top.next(), top.next()) {
        (Some((g, _)), None) => Some(g),
        _ => None,
    }
}

/// Whether `cwd` is `folder` or under it.
fn within(folder: &str, cwd: &str) -> bool {
    let folder = folder.trim_end_matches('/');
    let cwd = cwd.trim_end_matches('/');
    !folder.is_empty()
        && (cwd == folder
            || cwd
                .strip_prefix(folder)
                .is_some_and(|rest| rest.starts_with('/')))
}

/// The group a segment of `w`'s folder or repository names, the deepest
/// segment first.
fn named<'a>(w: &Work, groups: &[Choice<'a>]) -> Option<(&'a str, Why)> {
    deepest(w.folder)
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
    path.rsplit(['/', '\\'])
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

    fn picked(work: &[Work], working: &[Working]) -> Option<(String, Why)> {
        pick(work, working, &GROUPS).map(|p| (p.group, p.why))
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
    }

    #[test]
    fn a_group_the_boss_doesnt_see_or_an_empty_name_never_wins() {
        let only_atlas = [GROUPS[1]];
        assert_eq!(pick(&[work("/w/mantis")], &[], &only_atlas), None);
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
            pick(&[work("/w/mantis")], &working, &only_atlas).map(|p| p.group),
            Some("g-atlas".into()),
            "the group it sees, however many it doesn't see work there"
        );
        assert_eq!(picked(&[work("/w/··/x")], &[]), None);
    }
}
