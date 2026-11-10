//! What Desk keeps to say whose scope a row is ([`crate::scope`]): every
//! sidebar member's group, from the person's one layout (a session is in a
//! group by its `session:` key, wherever it runs), the groups there are,
//! and the bosses the hub said there are, each with its scope. Kept apart from the rest of the
//! desk's state, behind a lock nothing else is held under, so an event can
//! be stamped with its group wherever it is noted.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::crew::{Event, Party, PartyKind};
use crate::floor::{self, BossView, FloorView, Who};
use crate::scope::{self, Scope, Seen};

use super::{local_agent_item, session_item, Desk, Layout};

/// An overseer, as the hub said last: its agent, what it is called, and
/// its scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boss {
    pub agent: String,
    pub name: String,
    pub scope: Scope,
    /// `watch`, `suggest` or `act`.
    pub level: String,
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
    /// Groups that stay boundaries whether or not their boss oversees now
    /// (its config doesn't load, is switched off, or its agent is gone):
    /// until the person takes the boss off them, their rows are the
    /// person's alone. Kept across restarts.
    pub(crate) bounded: HashSet<String>,
    /// When each group that was a boundary stopped being one: the main
    /// overseer sees its rows from then on, never its past. Kept too.
    opened: HashMap<String, DateTime<Utc>>,
    /// Sessions being put in their group as they start: not a move.
    pub(crate) placing: HashSet<uuid::Uuid>,
    /// Sessions a confirmed move is moving: it does what the move does,
    /// not what a drag would.
    pub(crate) moving: HashSet<uuid::Uuid>,
}

/// The detail key an event names its group by when it is noted of one
/// scope on purpose: a string, or null for ungrouped.
pub(crate) const IN_GROUP: &str = "inGroup";

/// Where the boundaries are kept across restarts.
const BOUNDED: &str = "desk.groups.bounded";
const OPENED: &str = "desk.groups.opened";

impl Scopes {
    fn party_group(&self, p: &Party) -> Option<String> {
        self.group_by(p.kind, &p.id)
    }

    /// The group what `kind` and `id` name is in now.
    fn group_by(&self, kind: PartyKind, id: &str) -> Option<String> {
        let boss = || {
            self.bosses
                .iter()
                .flatten()
                .find(|b| b.agent == id)
                .map(|b| b.scope.group().map(str::to_string))
        };
        match kind {
            PartyKind::Session => id
                .parse()
                .ok()
                .and_then(|id| self.members.get(&session_item(id)).cloned()),
            // A boss's own doing is its scope's, wherever its agent sits; an
            // agent of another hub is known as `hub/agent`.
            PartyKind::Agent => boss().unwrap_or_else(|| {
                let item = match id.contains('/') {
                    true => format!("agent:{id}"),
                    false => local_agent_item(id),
                };
                self.members.get(&item).cloned()
            }),
            PartyKind::Manager => boss().flatten(),
            PartyKind::You | PartyKind::Desk => None,
        }
    }

    /// The group a Floor item about `who` is in now: its session's or
    /// agent's, a boss's own work its scope's, and a group's budget held by
    /// the breaker that group's.
    fn floor_group(&self, who: &Who) -> Option<String> {
        match who.kind {
            PartyKind::Desk => who
                .id
                .strip_prefix("pool:group:")
                .filter(|g| self.groups.contains_key(*g))
                .map(str::to_string),
            // The hub's own agents have no `hub/` in their ids: one that
            // does is no agent of this hub, and is placed by Desk.
            PartyKind::Agent | PartyKind::Manager if who.id.contains('/') => None,
            kind => self.group_by(kind, &who.id),
        }
    }

    /// The groups closed to everyone but the person: named for a boss, with
    /// no boss overseeing them now, and still in the sidebar.
    fn sealed(&self) -> Vec<String> {
        let bossing: HashSet<&str> = self
            .bosses
            .iter()
            .flatten()
            .filter_map(|b| b.scope.group())
            .collect();
        let mut sealed: Vec<String> = self
            .bounded
            .iter()
            .filter(|g| !bossing.contains(g.as_str()) && self.groups.contains_key(*g))
            .cloned()
            .collect();
        sealed.sort();
        sealed
    }

    fn bossed(&self) -> HashSet<String> {
        self.bosses
            .iter()
            .flatten()
            .filter_map(|b| b.scope.group().map(str::to_string))
            .chain(self.bounded.iter().cloned())
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

    /// Reads the boundaries kept across restarts.
    pub(super) async fn scopes_load(&self) {
        let read = |key: &'static str| async move {
            self.0
                .store
                .state(key)
                .await
                .inspect_err(
                    |e| tracing::warn!(error = %e, "reading the groups' boundaries failed"),
                )
                .ok()
                .flatten()
                .map(|e| e.value)
        };
        let bounded = read(BOUNDED).await;
        let opened = read(OPENED).await;
        let mut s = self.0.scopes.lock();
        s.bounded = bounded
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        s.opened = opened
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
    }

    /// Keeps the boundaries as they are now, for the next start.
    pub async fn keep_boundaries(&self) {
        self.scopes_keep().await;
    }

    async fn scopes_keep(&self) {
        let (bounded, opened) = {
            let s = self.0.scopes.lock();
            (serde_json::json!(s.bounded), serde_json::json!(s.opened))
        };
        for (key, value) in [(BOUNDED, bounded), (OPENED, opened)] {
            if let Err(e) = self.0.store.put_state(key, &value).await {
                tracing::error!(error = %e, "keeping the groups' boundaries failed");
            }
        }
    }

    /// Makes `groups` boundaries, as any agent's `[oversight]` names them:
    /// they stay so until the person takes the boss off them.
    pub async fn bound_groups(&self, groups: impl IntoIterator<Item = String>) {
        if self.bound_groups_now(groups) {
            self.scopes_keep().await;
        }
    }

    /// [`Self::bound_groups`] at once, in memory: whether any is new, and so
    /// is to be kept.
    pub fn bound_groups_now(&self, groups: impl IntoIterator<Item = String>) -> bool {
        let mut s = self.0.scopes.lock();
        let mut added = false;
        for g in groups {
            added |= s.bounded.insert(g);
        }
        added
    }

    /// Takes the boss off `group` through the hub's crew, before the group
    /// is deleted; nothing to do where there's no crew.
    pub async fn release_group(&self, group: &str) -> Result<(), String> {
        let crew = self
            .0
            .line_crew
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        match crew {
            Some(c) => c.release_group(group).await,
            None => Ok(()),
        }
    }

    /// `group` was closed for a boss whose config was then not written, and
    /// no config names it: it is as it was, open, its past too.
    pub async fn forget_bound(&self, group: &str) {
        let removed = self.0.scopes.lock().bounded.remove(group);
        if removed {
            self.scopes_keep().await;
        }
    }

    /// The groups closed to everyone but the person: named for a boss, with
    /// no boss overseeing them now (its config is off, or doesn't load).
    pub fn sealed(&self) -> Vec<String> {
        self.0.scopes.lock().sealed()
    }

    /// `view` with each need and piece of work stamped with the group it is
    /// in now and that group's own boss, and the sealed groups said: for
    /// the person, who sees across groups.
    pub fn stamp_floor(&self, view: &mut FloorView) {
        let s = self.0.scopes.lock();
        let bossing: HashMap<&str, &str> = s
            .bosses
            .iter()
            .flatten()
            .filter_map(|b| Some((b.scope.group()?, b.agent.as_str())))
            .collect();
        floor::stamp(
            view,
            |who| s.floor_group(who),
            |group| bossing.get(group).map(|a| a.to_string()),
        );
        view.sealed = s.sealed();
    }

    /// The overseers there are, as the Floor heads their sections: whether
    /// each is in a turn on `view`, what holds it, and its group's budget.
    pub async fn floor_bosses(&self, view: &FloorView) -> Vec<BossView> {
        let bosses = self.bosses();
        if bosses.is_empty() {
            return Vec::new();
        }
        let spend = self.group_standings().await;
        bosses
            .into_iter()
            .map(|b| BossView {
                state: floor::boss_state(view, &b.agent),
                paused: self.agent_held(&b.agent).filter(|why| !why.is_empty()),
                spend: b.scope.group().and_then(|g| spend.get(g).cloned()),
                scope: b.scope,
                agent: b.agent,
                name: b.name,
                level: b.level,
            })
            .collect()
    }

    /// The person took the boss off `group`: the main overseer sees it from
    /// now on, never what it held before.
    pub async fn unbound_group(&self, group: &str) {
        let removed = {
            let mut s = self.0.scopes.lock();
            let removed = s.bounded.remove(group);
            if removed {
                s.opened.insert(group.to_string(), Utc::now());
            }
            removed
        };
        if removed {
            self.scopes_keep().await;
        }
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
                level: o.level,
            })
            .into_iter()
            .collect()
    }

    /// Each overseer as a room member (`local/<agent>`), with its scope's
    /// group.
    pub(crate) fn boss_members(&self) -> HashMap<String, Option<String>> {
        self.bosses()
            .into_iter()
            .map(|b| {
                (
                    format!("{}/{}", crate::rooms::LOCAL_HUB, b.agent),
                    b.scope.group().map(str::to_string),
                )
            })
            .collect()
    }

    /// The group the agent `id` counts in: an overseer's scope's, wherever
    /// it sits; another agent's sidebar group's.
    pub fn group_of_agent(&self, id: &str) -> Option<String> {
        self.0.scopes.lock().party_group(&Party::agent(id, id))
    }

    /// The group one of this hub's agents counts in, by the scope rule: a
    /// boss's its scope's, wherever it sits; another's its sidebar group's.
    /// None for an id with a `hub/` in it, which is no agent of this hub.
    pub(crate) fn group_of_local_agent(&self, id: &str) -> Option<String> {
        if id.contains('/') {
            return None;
        }
        self.0.scopes.lock().group_by(PartyKind::Agent, id)
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
        // A boss served from a grant reaches its live grant's group alone.
        if crate::grant::is_served(agent) {
            return group.is_some_and(|g| {
                self.grant_live(g)
                    .is_ok_and(|grant| grant.boss.is_some_and(|b| b.party_id() == agent))
            });
        }
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

    /// The groups no overseer but their own sees: those with a boss, and
    /// those whose boss isn't overseeing now but was never taken off.
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
        let mut seen = Seen::new(scope.clone(), bossed, open);
        seen.opened = s.opened.clone();
        seen
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

    /// Every sidebar group's id and name.
    pub fn group_names(&self) -> Vec<(String, String)> {
        self.0
            .scopes
            .lock()
            .groups
            .iter()
            .map(|(id, name)| (id.clone(), name.clone()))
            .collect()
    }

    /// Stamps `e` with the group it is about now: its subject's, or with
    /// none its actor's. A ticket's event carries its board's group already.
    pub(super) fn stamp(&self, e: &mut Event) {
        // A ticket's events are its board's group's, and a plan's its own
        // group's, whoever they name.
        let of_plan = e.detail.as_ref().is_some_and(|d| d.get("plan").is_some());
        if e.ticket.is_some() || of_plan {
            return;
        }
        // An event said of one scope on purpose (a move is noted in both).
        if let Some(g) = e.detail.as_ref().and_then(|d| d.get(IN_GROUP)) {
            e.group = g.as_str().map(str::to_string);
            return;
        }
        let s = self.0.scopes.lock();
        e.group = match &e.subject {
            Some(subject) => s.party_group(subject),
            None => s.party_group(&e.actor),
        };
    }
}
