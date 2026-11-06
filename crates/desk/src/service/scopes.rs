//! What Desk keeps to say whose scope a row is ([`crate::scope`]): every
//! sidebar member's group, from the person's one layout (a session is in a
//! group by its `session:` key, wherever it runs), the groups there are,
//! and the bosses the hub said there are, each with its scope. Kept apart from the rest of the
//! desk's state, behind a lock nothing else is held under, so an event can
//! be stamped with its group wherever it is noted.

use std::collections::{HashMap, HashSet};

use crate::crew::{Event, Party, PartyKind};
use crate::scope::{self, Scope, Seen};

use super::{local_agent_item, session_item, Desk, Layout};

/// An overseer, as the hub said last: its agent, what it is called, and
/// its scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boss {
    pub agent: String,
    pub name: String,
    pub scope: Scope,
}

#[derive(Debug, Default)]
pub(crate) struct Scopes {
    /// Each sidebar member's group, by its key; Ungrouped's are left out.
    members: HashMap<String, String>,
    /// The sidebar's groups' names, by id.
    groups: HashMap<String, String>,
    /// None until the hub says: then the crew's one overseer is the main
    /// one, and no group has a boss.
    bosses: Option<Vec<Boss>>,
}

impl Scopes {
    fn party_group(&self, p: &Party) -> Option<String> {
        let boss = || {
            self.bosses
                .iter()
                .flatten()
                .find(|b| b.agent == p.id)
                .map(|b| b.scope.group().map(str::to_string))
        };
        match p.kind {
            PartyKind::Session => {
                p.id.parse()
                    .ok()
                    .and_then(|id| self.members.get(&session_item(id)).cloned())
            }
            // A boss's own doing is its scope's, wherever its agent sits.
            PartyKind::Agent => {
                boss().unwrap_or_else(|| self.members.get(&local_agent_item(&p.id)).cloned())
            }
            PartyKind::Manager => boss().flatten(),
            PartyKind::You | PartyKind::Desk => None,
        }
    }

    fn bossed(&self) -> HashSet<String> {
        self.bosses
            .iter()
            .flatten()
            .filter_map(|b| b.scope.group().map(str::to_string))
            .collect()
    }
}

impl Desk {
    /// Keeps the members' groups in step with `layout`.
    pub(super) fn scopes_follow(&self, layout: &Layout) {
        let mut s = self.0.scopes.lock();
        s.members = layout
            .groups
            .iter()
            .flat_map(|g| g.items.iter().map(|i| (i.clone(), g.id.clone())))
            .collect();
        s.groups = layout
            .groups
            .iter()
            .map(|g| (g.id.clone(), g.name.clone()))
            .collect();
    }

    /// The overseers there are now, each with its scope: the hub says, each
    /// time it reads its agents' `[oversight]`.
    pub fn set_bosses(&self, bosses: Vec<Boss>) {
        self.0.scopes.lock().bosses = Some(bosses);
    }

    /// The overseers there are: as the hub said last, else the crew's one
    /// overseer, over everything.
    pub fn bosses(&self) -> Vec<Boss> {
        if let Some(said) = self.0.scopes.lock().bosses.clone() {
            return said;
        }
        let crew = self
            .0
            .line_crew
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        crew.and_then(|c| c.overseer())
            .map(|o| Boss {
                agent: o.id,
                name: o.name,
                scope: Scope::Ungrouped,
            })
            .into_iter()
            .collect()
    }

    /// The boss of `scope`, when it has one.
    pub fn boss_of(&self, scope: &Scope) -> Option<Boss> {
        self.bosses().into_iter().find(|b| b.scope == *scope)
    }

    /// The scope the overseer `agent` oversees, while it does.
    pub fn scope_of_boss(&self, agent: &str) -> Option<Scope> {
        self.bosses()
            .into_iter()
            .find(|b| b.agent == agent)
            .map(|b| b.scope)
    }

    /// Whether the boss `agent` may reach what is in `group`: what its scope
    /// sees, once the hub has said who the bosses are (before, there is one
    /// scope). One that isn't a boss reaches nothing.
    pub(crate) fn boss_reaches(&self, agent: &str, group: Option<&str>) -> bool {
        let s = self.0.scopes.lock();
        let Some(bosses) = &s.bosses else {
            return true;
        };
        let bossed = s.bossed();
        bosses
            .iter()
            .find(|b| b.agent == agent)
            .is_some_and(|b| scope::sees(&b.scope, group, &bossed))
    }

    /// The groups no overseer but their own sees: those with a boss.
    pub fn bossed(&self) -> HashSet<String> {
        self.0.scopes.lock().bossed()
    }

    /// What an overseer of `scope` sees, as things stand.
    pub fn seen_by(&self, scope: &Scope) -> Seen {
        let s = self.0.scopes.lock();
        let bossed = s.bossed();
        let open = s
            .groups
            .keys()
            .filter(|g| !bossed.contains(*g))
            .cloned()
            .collect();
        Seen::new(scope.clone(), bossed, open)
    }

    /// The scope a row of `group` is in now.
    pub fn scope_of_group(&self, group: Option<&str>) -> Scope {
        scope::scope_of(group, &self.bossed())
    }

    /// The group the sidebar member `item` is in now.
    pub fn group_of_member(&self, item: &str) -> Option<String> {
        self.0.scopes.lock().members.get(item).cloned()
    }

    /// Whether `id` is one of the sidebar's groups now.
    pub fn is_group(&self, id: &str) -> bool {
        self.0.scopes.lock().groups.contains_key(id)
    }

    /// What the group `id` is called.
    pub fn group_name(&self, id: &str) -> Option<String> {
        self.0.scopes.lock().groups.get(id).cloned()
    }

    /// Stamps `e` with the group it is about now: its subject's, or with
    /// none its actor's. A ticket's event carries its board's group already.
    pub(super) fn stamp(&self, e: &mut Event) {
        if e.ticket.is_some() {
            return;
        }
        let s = self.0.scopes.lock();
        e.group = match &e.subject {
            Some(subject) => s.party_group(subject),
            None => s.party_group(&e.actor),
        };
    }
}
