//! The group a plan's work belongs to when nothing names one, read from its
//! tickets: each votes for at most one group, by the first of the session
//! it takes on, the sessions working in its folder, and a group its
//! repository names (its root folder, its origin's owner and name, or its
//! folders below the root; never the folders it sits in). The group with
//! the most votes wins; a tie or no vote picks none.

use std::collections::BTreeMap;

/// One ticket of a plan, as its group is read from it.
#[derive(Debug, Clone, Copy)]
pub struct Work<'a> {
    pub folder: &'a str,
    /// The repository it names, `owner/name` or a URL.
    pub repo: Option<&'a str>,
    /// The group of the session it takes on, when that session is in one.
    pub session: Option<&'a str>,
    /// The repository its folder is in, when there is one.
    pub checkout: Option<Checkout<'a>>,
}

/// The repository a ticket's folder is in.
#[derive(Debug, Clone, Copy)]
pub struct Checkout<'a> {
    /// Its root folder's name (a worktree's: the repository it was made
    /// from).
    pub root: &'a str,
    /// Its origin remote, `host/owner/name`.
    pub remote: Option<&'a str>,
    /// The ticket's folder below the checkout's top; empty at the top.
    pub below: &'a str,
}

/// A [`Checkout`] as it was read from disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    pub root: String,
    pub remote: Option<String>,
    pub below: String,
}

impl Found {
    pub fn checkout(&self) -> Checkout<'_> {
        Checkout {
            root: &self.root,
            remote: self.remote.as_deref(),
            below: &self.below,
        }
    }
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
    /// Its repository names the group, by this name.
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
    most(counts, |n| *n).map(|(g, _)| g)
}

const SEPARATORS: [char; 2] = ['/', '\\'];

/// Whether `cwd` is `folder` or under it.
fn within(folder: &str, cwd: &str) -> bool {
    let folder = folder.trim_end_matches(SEPARATORS);
    let cwd = cwd.trim_end_matches(SEPARATORS);
    !folder.is_empty()
        && cwd
            .strip_prefix(folder)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(SEPARATORS))
}

/// The group `w`'s repository names: its folders below the root (the
/// deepest first), the root, its origin's name and owner, then the
/// repository the ticket names.
fn named<'a>(w: &Work, groups: &[Choice<'a>]) -> Option<(&'a str, Why)> {
    w.checkout
        .iter()
        .flat_map(|c| {
            deepest(c.below)
                .chain([c.root])
                .chain(name_and_owner(c.remote))
        })
        .chain(name_and_owner(w.repo))
        .find_map(|seg| {
            groups
                .iter()
                .find(|g| names(seg, g.name))
                .map(|g| (g.id, Why::Named(seg.to_string())))
        })
}

/// A repository's name, then its owner.
fn name_and_owner(repo: Option<&str>) -> impl Iterator<Item = &str> {
    repo.into_iter().flat_map(|r| deepest(r).take(2))
}

/// `path`'s segments, the deepest first, without a `.git` ending.
fn deepest(path: &str) -> impl Iterator<Item = &str> {
    path.rsplit(SEPARATORS)
        .map(|s| s.strip_suffix(".git").unwrap_or(s))
        .filter(|s| !s.is_empty())
}

/// Whether `seg` names the group called `name`: the same letters and
/// digits, or those of its first words (`mantis-crm` names Mantis).
fn names(seg: &str, name: &str) -> bool {
    let name = fold(name);
    !name.is_empty()
        && seg
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .scan(String::new(), |words, w| {
                words.push_str(&fold(w));
                Some(words.clone())
            })
            .any(|words| words == name)
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
            checkout: None,
        }
    }

    /// `folder`, the top of a checkout of `root` from `remote`.
    fn checked_out<'a>(folder: &'a str, root: &'a str, remote: Option<&'a str>) -> Work<'a> {
        Work {
            checkout: Some(Checkout {
                root,
                remote,
                below: "",
            }),
            ..work(folder)
        }
    }

    fn picked(work: &[Work], working: &[Working]) -> Option<(String, Why)> {
        pick(work, working, &GROUPS).map(|p| (p.group, p.why))
    }

    #[test]
    fn a_tickets_session_then_its_folder_then_its_repository_names_its_group() {
        let mono = "/w/mantis-crm/monorepo";
        let remote = Some("github.com/mantis-crm/monorepo");
        let working = [Working {
            cwd: "/w/mantis-crm/monorepo/.worktrees/man-1",
            group: "g-atlas",
        }];
        let by_session = Work {
            session: Some("g-mantis"),
            ..checked_out(mono, "monorepo", remote)
        };
        assert_eq!(
            picked(&[by_session], &working),
            Some(("g-mantis".into(), Why::Session))
        );
        assert_eq!(
            picked(&[checked_out(mono, "monorepo", remote)], &working),
            Some(("g-atlas".into(), Why::Folder(mono.into())))
        );
        assert_eq!(
            picked(&[checked_out(mono, "monorepo", remote)], &[]),
            Some(("g-mantis".into(), Why::Named("mantis-crm".into())))
        );
        assert_eq!(
            picked(&[checked_out("/w/x", "atlas", None)], &[]),
            Some(("g-atlas".into(), Why::Named("atlas".into())))
        );
        let below = Work {
            checkout: Some(Checkout {
                root: "monorepo",
                remote,
                below: "services/atlas",
            }),
            ..work("/w/mantis-crm/monorepo/services/atlas")
        };
        assert_eq!(
            picked(&[below], &[]),
            Some(("g-atlas".into(), Why::Named("atlas".into()))),
            "a folder below the root before the root and its origin"
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
            ..checked_out("/w/atlas", "atlas", None)
        };
        assert_eq!(
            picked(&[unknown_session], &[]),
            Some(("g-atlas".into(), Why::Named("atlas".into())))
        );
    }

    #[test]
    fn the_folders_a_repository_sits_in_never_name_its_group() {
        let groups = [
            Choice {
                id: "g-dev",
                name: "Dev",
            },
            Choice {
                id: "g-projects",
                name: "Projects",
            },
            Choice {
                id: "g-mantis",
                name: "Mantis",
            },
        ];
        let folder = "/Users/kings/dev/projects/mantis-crm/monorepo";
        let at = |w: Work| pick(&[w], &[], &groups).map(|p| (p.group, p.why));
        assert_eq!(
            at(checked_out(
                folder,
                "monorepo",
                Some("github.com/mantis-crm/monorepo")
            )),
            Some(("g-mantis".into(), Why::Named("mantis-crm".into())))
        );
        assert_eq!(at(checked_out(folder, "monorepo", None)), None);
        assert_eq!(at(work(folder)), None, "no repository: no name");
    }

    #[test]
    fn a_name_is_the_whole_segment_or_its_first_words() {
        assert!(names("mantis-crm", "Mantis"));
        assert!(names("mantis_crm", "Mantis CRM"));
        assert!(names("Mantis", "mantis"));
        assert!(!names("crm-mantis", "Mantis"));
        assert!(!names("mantiscrm", "Mantis"));
        assert!(!names("anything", "··"));
    }

    #[test]
    fn the_most_votes_win_and_a_tie_or_nothing_picks_no_group() {
        let two = [
            checked_out("/w/a", "mantis-a", None),
            checked_out("/w/b", "mantis-b", None),
            checked_out("/w/c", "atlas", None),
        ];
        assert_eq!(picked(&two, &[]).map(|(g, _)| g), Some("g-mantis".into()));
        let tie = [
            checked_out("/w/a", "mantis", None),
            checked_out("/w/c", "atlas", None),
        ];
        assert_eq!(picked(&tie, &[]), None);
        assert_eq!(picked(&[checked_out("/w/o", "other", None)], &[]), None);
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
    fn a_group_the_boss_doesnt_see_never_wins() {
        let only_atlas = [GROUPS[1]];
        assert_eq!(
            pick(&[checked_out("/w/m", "mantis", None)], &[], &only_atlas),
            None
        );
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
    }
}
