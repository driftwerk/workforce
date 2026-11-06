//! One rule for what an overseer's scope sees. A scope is everything
//! ungrouped, or one sidebar group ([`Scope`]). A group with a boss of its
//! own is that boss's alone: the main overseer sees nothing inside it, and
//! its boss sees nothing outside it, so one client's data never shares a
//! context or a memory with another's. A group with no boss is the main
//! overseer's, as ungrouped items are. The person sees everything.
//!
//! Rows are stamped with their group when written, not when read: moving
//! an item later never moves what was written about it. A row of a group
//! no longer in the sidebar (deleted since) is no overseer's: only the
//! person sees it.
//!
//! An item's group is always read from the person's one sidebar layout, by
//! its key, never from where it runs: a session on another machine, reached
//! through another Desk hub, would sit in a group by its own `session:` key
//! and be stamped and scoped as any other.

use std::collections::HashSet;

pub use workforce_core::scope::Scope;

use crate::service::{Layout, UNGROUPED};

/// The group an item (`session:<id>`, `agent:<hub>/<agent>`) is in, from
/// the layout; none in Ungrouped.
pub fn group_of<'a>(item: &str, layout: &'a Layout) -> Option<&'a str> {
    layout.group_of(item).map(|g| g.id.as_str())
}

/// Whose a row stamped with `group` is: its group's when that group has a
/// boss (`bossed`), else the main overseer's.
pub fn scope_of(group: Option<&str>, bossed: &HashSet<String>) -> Scope {
    match group.filter(|g| *g != UNGROUPED) {
        Some(g) if bossed.contains(g) => Scope::Group(g.to_string()),
        _ => Scope::Ungrouped,
    }
}

/// Whether an overseer of `scope` may see a row stamped with `group`.
pub fn sees(scope: &Scope, group: Option<&str>, bossed: &HashSet<String>) -> bool {
    scope_of(group, bossed) == *scope
}

/// What one overseer sees, as things stood when it was read: its scope,
/// the groups with a boss of their own, and the sidebar's groups with none
/// (`open`), which are the main overseer's. A group in neither is no
/// longer in the sidebar: no overseer sees its rows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Seen {
    pub scope: Scope,
    pub bossed: HashSet<String>,
    pub open: HashSet<String>,
}

impl Seen {
    pub fn new(scope: Scope, bossed: HashSet<String>, open: HashSet<String>) -> Self {
        Self {
            scope,
            bossed,
            open,
        }
    }

    /// Whether it sees a row stamped with `group`: [`sees`], and never a
    /// group that is no longer in the sidebar.
    pub fn sees(&self, group: Option<&str>) -> bool {
        let gone = group.is_some_and(|g| !self.bossed.contains(g) && !self.open.contains(g));
        !gone && sees(&self.scope, group, &self.bossed)
    }

    /// The scope's group, a query's first parameter for [`seen_sql`].
    pub fn group_param(&self) -> Option<&str> {
        self.scope.group()
    }

    /// The groups with no boss, a query's second parameter for [`seen_sql`].
    pub fn open_param(&self) -> Vec<String> {
        let mut o: Vec<String> = self.open.iter().cloned().collect();
        o.sort();
        o
    }
}

/// [`Seen::sees`] in SQL, over `column`, with the scope's group as
/// parameter `$group` (null: ungrouped) and the groups with no boss as
/// `$open`.
pub fn seen_sql(column: &str, group: usize, open: usize) -> String {
    format!(
        "(CASE WHEN ${group}::text IS NOT NULL THEN {column} = ${group}::text \
         ELSE ({column} IS NULL OR {column} = ANY(${open}::text[])) END)"
    )
}

/// The refusal an overseer reads for an id outside its scope, named by its
/// group (`None`: the main overseer). The same words for an id that
/// doesn't exist, so a refusal never says what another scope holds.
pub fn not_ours(group_name: Option<&str>) -> String {
    match group_name {
        Some(name) => format!("That isn't one of {name}'s."),
        None => "That isn't one of yours: it is in a group with a boss of its own.".into(),
    }
}

/// The sidebar key of a room member (`hub/agent`).
pub fn member_item(member: &crate::rooms::Member) -> String {
    format!("agent:{}", member.key())
}

/// Whether `member` may be in a room of `room_group` (none: ungrouped),
/// given the layout and the groups with a boss of their own. A bossed
/// group's room takes that group's agents, and local agents of no other
/// bossed group; any other room takes agents of any hub that are in no
/// bossed group, because a bossed group's agents join only their group's
/// rooms.
pub fn allowed_member(
    room_group: Option<&str>,
    member: &crate::rooms::Member,
    layout: &Layout,
    bossed: &HashSet<String>,
) -> bool {
    let item = member_item(member);
    let theirs = group_of(&item, layout).filter(|g| bossed.contains(*g));
    match room_group.filter(|g| bossed.contains(*g)) {
        Some(room) => match theirs {
            Some(g) => g == room,
            None => member.hub == crate::rooms::LOCAL_HUB,
        },
        None => theirs.is_none(),
    }
}

/// The hubs `members` reach that neither `before`'s members' hubs nor the
/// room's acknowledged `shared` already have: a room's conversation goes to
/// every member's hub, so a new one is asked about once. Nothing is asked
/// while every member is on one hub.
pub fn unshared_hubs(
    before: &[crate::rooms::Member],
    members: &[crate::rooms::Member],
    shared: &[String],
) -> Vec<String> {
    let mut after: Vec<&str> = members.iter().map(|m| m.hub.as_str()).collect();
    after.sort_unstable();
    after.dedup();
    if after.len() < 2 {
        return Vec::new();
    }
    after
        .into_iter()
        .filter(|h| !before.iter().any(|m| m.hub == *h) && !shared.iter().any(|s| s == h))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooms::Member;
    use crate::service::Group;

    fn layout() -> Layout {
        Layout {
            groups: vec![
                Group {
                    id: "acme".into(),
                    name: "Acme".into(),
                    collapsed: false,
                    items: vec![
                        "session:00000000-0000-0000-0000-000000000001".into(),
                        "agent:local/sarah".into(),
                        "agent:atlas-hub/ada".into(),
                    ],
                },
                Group {
                    id: "atlas".into(),
                    name: "Atlas".into(),
                    collapsed: false,
                    items: vec!["agent:local/mia".into()],
                },
                Group {
                    id: "infra".into(),
                    name: "Infra".into(),
                    collapsed: false,
                    items: vec!["agent:local/scout".into()],
                },
            ],
            ..Layout::default()
        }
    }

    fn bossed() -> HashSet<String> {
        ["acme".to_string(), "atlas".to_string()].into()
    }

    #[test]
    fn a_bossed_groups_rows_are_its_bosses_and_the_rest_the_main_overseers() {
        let b = bossed();
        let acme = Scope::Group("acme".into());
        let main = Scope::Ungrouped;
        assert_eq!(scope_of(Some("acme"), &b), acme);
        assert_eq!(scope_of(Some("infra"), &b), main, "a group with no boss");
        assert_eq!(scope_of(None, &b), main);
        assert_eq!(scope_of(Some(UNGROUPED), &b), main);
        assert!(sees(&acme, Some("acme"), &b));
        assert!(!sees(&acme, Some("atlas"), &b));
        assert!(
            !sees(&acme, None, &b),
            "a group's boss sees nothing outside it"
        );
        assert!(!sees(&acme, Some("infra"), &b));
        assert!(
            !sees(&main, Some("acme"), &b),
            "nothing inside a bossed group"
        );
        assert!(sees(&main, Some("infra"), &b), "a group with no boss");
        assert!(sees(&main, None, &b));
        assert!(
            !sees(&Scope::Group("infra".into()), Some("infra"), &b),
            "a group nobody bosses has no scope of its own"
        );
        let l = layout();
        assert_eq!(
            group_of("session:00000000-0000-0000-0000-000000000001", &l),
            Some("acme")
        );
        assert_eq!(group_of("agent:local/nobody", &l), None);
    }

    #[test]
    fn a_rooms_members_keep_to_its_boundary() {
        let (l, b) = (layout(), bossed());
        let m = |hub: &str, agent: &str| Member {
            hub: hub.into(),
            agent: agent.into(),
        };
        // Acme's room: its own agents, and local ones of no other bossed group.
        assert!(allowed_member(Some("acme"), &m("local", "sarah"), &l, &b));
        assert!(allowed_member(Some("acme"), &m("atlas-hub", "ada"), &l, &b));
        assert!(allowed_member(Some("acme"), &m("local", "scout"), &l, &b));
        assert!(allowed_member(Some("acme"), &m("local", "free"), &l, &b));
        assert!(!allowed_member(Some("acme"), &m("local", "mia"), &l, &b));
        assert!(!allowed_member(Some("acme"), &m("other", "zed"), &l, &b));
        // An ungrouped room (or one of a group with no boss): no bossed agent.
        for room in [None, Some("infra")] {
            assert!(allowed_member(room, &m("other", "zed"), &l, &b));
            assert!(allowed_member(room, &m("local", "scout"), &l, &b));
            assert!(!allowed_member(room, &m("local", "sarah"), &l, &b));
            assert!(!allowed_member(room, &m("atlas-hub", "ada"), &l, &b));
        }
    }

    #[test]
    fn a_new_hub_in_a_room_is_asked_about_once() {
        let m = |hub: &str| Member {
            hub: hub.into(),
            agent: "a".into(),
        };
        assert!(unshared_hubs(&[], &[m("local")], &[]).is_empty());
        assert!(unshared_hubs(&[m("local")], &[m("local"), m("local")], &[]).is_empty());
        assert_eq!(
            unshared_hubs(&[m("local")], &[m("local"), m("atlas")], &[]),
            ["atlas"]
        );
        assert!(
            unshared_hubs(&[m("local")], &[m("local"), m("atlas")], &["atlas".into()]).is_empty()
        );
        assert_eq!(
            unshared_hubs(&[], &[m("acme"), m("atlas")], &[]),
            ["acme", "atlas"],
            "a new room with two hubs asks about both"
        );
    }

    #[test]
    fn the_refusal_names_the_scope_and_never_what_is_outside_it() {
        assert_eq!(not_ours(Some("Acme")), "That isn't one of Acme's.");
        assert!(not_ours(None).contains("boss of its own"));
        let sql = seen_sql("group_id", 2, 3);
        assert!(
            sql.contains("$2::text") && sql.contains("$3::text[]"),
            "{sql}"
        );
    }

    #[test]
    fn a_group_gone_from_the_sidebar_is_no_overseers() {
        let open: HashSet<String> = ["infra".to_string()].into();
        let main = Seen::new(Scope::Ungrouped, bossed(), open.clone());
        assert!(main.sees(None) && main.sees(Some("infra")));
        assert!(!main.sees(Some("acme")) && !main.sees(Some("deleted")));
        let acme = Seen::new(Scope::Group("acme".into()), bossed(), open);
        assert!(acme.sees(Some("acme")) && !acme.sees(Some("deleted")));
    }
}
