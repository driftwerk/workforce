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

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

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
/// the groups that are boundaries (a boss of their own, or one named for
/// them that isn't overseeing now), the sidebar's groups that aren't
/// (`open`), which are the main overseer's, and when each group that once
/// was a boundary stopped being one (`opened`): the main overseer sees its
/// rows from then on, never its past. A group in neither set is no longer in
/// the sidebar: no overseer sees its rows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Seen {
    pub scope: Scope,
    pub bossed: HashSet<String>,
    pub open: HashSet<String>,
    pub opened: HashMap<String, DateTime<Utc>>,
}

impl Seen {
    pub fn new(scope: Scope, bossed: HashSet<String>, open: HashSet<String>) -> Self {
        Self {
            scope,
            bossed,
            open,
            opened: HashMap::new(),
        }
    }

    /// A view of nothing at all: for a caller Desk can't place.
    pub fn nothing() -> Self {
        Self::new(Scope::Group(String::new()), HashSet::new(), HashSet::new())
    }

    /// Whether it sees what is in `group` now: [`sees`], and never a group
    /// that is no longer in the sidebar.
    pub fn sees(&self, group: Option<&str>) -> bool {
        let gone = group.is_some_and(|g| !self.bossed.contains(g) && !self.open.contains(g));
        !gone && sees(&self.scope, group, &self.bossed)
    }

    /// Whether it sees a row stamped with `group` at `at`: as [`Self::sees`],
    /// and for the main overseer, of a group that was a boundary, only a row
    /// written after it stopped being one.
    pub fn sees_row(&self, group: Option<&str>, at: DateTime<Utc>) -> bool {
        let before = group
            .and_then(|g| self.opened.get(g))
            .is_some_and(|since| at < *since);
        self.sees(group) && !(self.scope == Scope::Ungrouped && before)
    }

    /// The parameters [`seen_sql`] takes, in order.
    pub fn params(&self) -> SeenParams {
        let mut open: Vec<String> = self.open.iter().cloned().collect();
        open.sort();
        let (groups, since): (Vec<String>, Vec<DateTime<Utc>>) =
            self.opened.iter().map(|(g, at)| (g.clone(), *at)).unzip();
        SeenParams {
            group: self.scope.group().map(str::to_string),
            open,
            groups,
            since,
        }
    }
}

/// [`Seen::sees_row`]'s parameters for SQL.
#[derive(Debug, Clone, Default)]
pub struct SeenParams {
    pub group: Option<String>,
    pub open: Vec<String>,
    pub groups: Vec<String>,
    pub since: Vec<DateTime<Utc>>,
}

/// [`Seen::sees_row`] in SQL, over `column` and the row's time `at`, with
/// [`SeenParams`] as parameters `$first` to `$first + 3`.
pub fn seen_sql(column: &str, at: &str, first: usize) -> String {
    let (g, open, og, ot) = (first, first + 1, first + 2, first + 3);
    format!(
        "(CASE WHEN ${g}::text IS NOT NULL THEN {column} = ${g}::text \
         ELSE ({column} IS NULL OR ({column} = ANY(${open}::text[]) AND NOT EXISTS ( \
           SELECT 1 FROM unnest(${og}::text[], ${ot}::timestamptz[]) AS o(g, t) \
           WHERE o.g = {column} AND {at} < o.t))) END)"
    )
}

/// The refusal an overseer reads for an id outside its scope, named by its
/// group (`None`: the main overseer). The same words for an id that
/// doesn't exist, so a refusal never says what another scope holds.
pub fn not_ours(group_name: Option<&str>) -> String {
    match group_name {
        Some(name) => format!("That isn't one of {name}'s."),
        None => "That isn't one of yours.".into(),
    }
}

/// What moving a session between groups does, beyond the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Its ticket on the old scope's board is taken off it, back to To do.
    Unassign { ticket: uuid::Uuid },
    /// Its open questions to the old boss are withdrawn.
    WithdrawAsks,
    /// The old boss stops running it.
    Release,
    /// It is told, on its line, what changed.
    Tell,
}

/// What moving a session from group `from` to group `to` does: nothing but
/// the sidebar within one scope; across a boss boundary, its `tickets` on
/// the old scope's boards are unassigned, its open questions to the old
/// boss withdrawn, the old boss lets it go when it ran it (`charged`), and
/// it is told. What it wrote stays the old group's; what it writes next is
/// the new one's.
pub fn move_effects(
    from: Option<&str>,
    to: Option<&str>,
    tickets: &[uuid::Uuid],
    charged: bool,
    bossed: &HashSet<String>,
) -> Vec<Effect> {
    if scope_of(from, bossed) == scope_of(to, bossed) {
        return Vec::new();
    }
    let mut out: Vec<Effect> = tickets
        .iter()
        .map(|t| Effect::Unassign { ticket: *t })
        .collect();
    out.push(Effect::WithdrawAsks);
    if charged {
        out.push(Effect::Release);
    }
    out.push(Effect::Tell);
    out
}

/// The sidebar key of a room member (`hub/agent`).
pub fn member_item(member: &crate::rooms::Member) -> String {
    format!("agent:{}", member.key())
}

/// Whether `member` may be in a room of `room_group` (none: ungrouped),
/// given the layout, the groups with a boss of their own, and the
/// overseers (`bosses`: each one's member key, `local/<agent>`, and its
/// scope's group). A bossed group's room takes that group's agents, its
/// boss, and local agents of no other bossed group that oversee nothing;
/// any other room takes agents of any hub that are in no bossed group, and
/// the main overseer, because a bossed group's agents and its boss join
/// only their group's rooms.
pub fn allowed_member(
    room_group: Option<&str>,
    member: &crate::rooms::Member,
    layout: &Layout,
    bossed: &HashSet<String>,
    bosses: &HashMap<String, Option<String>>,
) -> bool {
    let item = member_item(member);
    let boss = bosses.get(&member.key());
    let theirs = match boss {
        Some(g) => g.as_deref(),
        None => group_of(&item, layout).filter(|g| bossed.contains(*g)),
    };
    match room_group.filter(|g| bossed.contains(*g)) {
        Some(room) => match theirs {
            Some(g) => g == room,
            None => boss.is_none() && member.hub == crate::rooms::LOCAL_HUB,
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
        let bosses: HashMap<String, Option<String>> = [
            ("local/desk".to_string(), None),
            ("local/sarah-boss".to_string(), Some("acme".to_string())),
            ("local/mia-boss".to_string(), Some("atlas".to_string())),
        ]
        .into();
        let allowed_member_ = |room: Option<&str>, m: &Member, l: &Layout, b: &HashSet<String>| {
            allowed_member(room, m, l, b, &bosses)
        };
        let m = |hub: &str, agent: &str| Member {
            hub: hub.into(),
            agent: agent.into(),
        };
        // Acme's room: its own agents, and local ones of no other bossed group.
        assert!(allowed_member_(Some("acme"), &m("local", "sarah"), &l, &b));
        assert!(allowed_member_(
            Some("acme"),
            &m("atlas-hub", "ada"),
            &l,
            &b
        ));
        assert!(allowed_member_(Some("acme"), &m("local", "scout"), &l, &b));
        assert!(allowed_member_(Some("acme"), &m("local", "free"), &l, &b));
        assert!(!allowed_member_(Some("acme"), &m("local", "mia"), &l, &b));
        assert!(!allowed_member_(Some("acme"), &m("other", "zed"), &l, &b));
        // A boss joins only its own group's rooms; the main overseer none of
        // a bossed group's.
        assert!(allowed_member_(
            Some("acme"),
            &m("local", "sarah-boss"),
            &l,
            &b
        ));
        assert!(!allowed_member_(
            Some("acme"),
            &m("local", "mia-boss"),
            &l,
            &b
        ));
        assert!(!allowed_member_(Some("acme"), &m("local", "desk"), &l, &b));
        assert!(allowed_member_(None, &m("local", "desk"), &l, &b));
        assert!(!allowed_member_(None, &m("local", "sarah-boss"), &l, &b));
        // An ungrouped room (or one of a group with no boss): no bossed agent.
        for room in [None, Some("infra")] {
            assert!(allowed_member_(room, &m("other", "zed"), &l, &b));
            assert!(allowed_member_(room, &m("local", "scout"), &l, &b));
            assert!(!allowed_member_(room, &m("local", "sarah"), &l, &b));
            assert!(!allowed_member_(room, &m("atlas-hub", "ada"), &l, &b));
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
        assert_eq!(not_ours(None), "That isn't one of yours.");
        let sql = seen_sql("group_id", "at", 2);
        assert!(
            sql.contains("$2::text") && sql.contains("$3::text[]"),
            "{sql}"
        );
        assert!(sql.contains("$4::text[], $5::timestamptz[]"), "{sql}");
    }

    #[test]
    fn a_move_across_a_boss_boundary_unassigns_withdraws_and_lets_go_and_within_one_changes_nothing(
    ) {
        let b = bossed();
        let t = uuid::Uuid::from_u128(7);
        assert!(
            move_effects(None, Some("infra"), &[t], true, &b).is_empty(),
            "one scope"
        );
        assert!(move_effects(Some("acme"), Some("acme"), &[t], true, &b).is_empty());
        assert_eq!(
            move_effects(Some("acme"), Some("atlas"), &[t], true, &b),
            [
                Effect::Unassign { ticket: t },
                Effect::WithdrawAsks,
                Effect::Release,
                Effect::Tell
            ]
        );
        assert_eq!(
            move_effects(None, Some("acme"), &[], false, &b),
            [Effect::WithdrawAsks, Effect::Tell],
            "into a bossed group, with no ticket, not run"
        );
        assert_eq!(
            move_effects(Some("atlas"), Some("infra"), &[t], false, &b),
            [
                Effect::Unassign { ticket: t },
                Effect::WithdrawAsks,
                Effect::Tell
            ],
            "out to the main overseer"
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

    #[test]
    fn a_group_that_was_a_boundary_opens_to_the_main_overseer_from_then_on_not_its_past() {
        let now = Utc::now();
        let mut main = Seen::new(
            Scope::Ungrouped,
            HashSet::new(),
            ["acme".to_string()].into(),
        );
        main.opened.insert("acme".into(), now);
        assert!(!main.sees_row(Some("acme"), now - chrono::Duration::seconds(1)));
        assert!(main.sees_row(Some("acme"), now));
        assert!(main.sees_row(None, now - chrono::Duration::days(1)));
    }
}
