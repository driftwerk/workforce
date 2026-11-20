//! The grants each of the person's machines should hold for this hub's own
//! group bosses (remote bosses §4.3): a local boss reaches its group's
//! machine sessions through the relay, and the machine decides each call
//! under its grant for the group. This hub works out what each grant says,
//! from its own state alone; Desk's native side only carries it to each
//! machine (`GET /api/desk/machine-grants`, the hub's own token from the
//! `Authorization` header alone, as the relay's routes are).
//!
//! Same rules, no wider powers: a grant says exactly what the boss may do
//! here, in the same group, and nothing more.
//!
//! - **Who:** each group's own boss ([`CrewHub::overseers`]), as
//!   `local:<agent>`. A group with no boss of its own, Ungrouped, a group
//!   named for a boss that oversees nothing now, and a group this hub
//!   holds a grant for whose boss is on another hub get none: one boss per
//!   group.
//! - **The main overseer's plans:** it runs no group, so each of its plans
//!   gets a grant of its own on each machine its tickets' sessions work on
//!   ([`plan_key`], never a sidebar group's id), holding exactly those
//!   sessions as the plan runner last read them, while it sees them in the
//!   sidebar. It goes once the plan has none there, or ends. It acts
//!   within Desk at most: Act fully's one budget is counted by group.
//! - **What it may do:** its level as chosen here (Act fully as chosen,
//!   the rest as it runs now, [`CrewHub::level_now`]), and its own `[oversight.act]` switches and
//!   `parallel`. At Act fully the grant carries its daily budget (with none
//!   set here, the most Act fully allows), and that budget is one across
//!   this computer and every machine ([`super::shared_budget`]): each
//!   machine acts fully only while this hub's take, at most
//!   [`super::relayed_decide::FRESH`] old, says so with what was spent
//!   everywhere else.
//! - **What it sees:** everything, terminals included, as its
//!   `crew.history` here shows them; the machine still redacts and caps
//!   every string.
//! - **Never more:** no folders (it starts no session there), no
//!   repositories (Act fully ships nowhere from a grant), no memory
//!   search, never a resume of a stop.
//! - **Which sessions:** that machine's sessions the sidebar has in the
//!   group, or the plan's tickets work in, and no other.
//!
//! Each machine's sessions the sidebar lists anywhere come too (`listed`),
//! so Desk can tell the machine which sessions born there in a group have
//! been taken out of it.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::Serialize;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_common::oversight::{Level, Oversight, Scope};
use workforce_desk::grant::{BossRef, MAX_MEMBERS};

use super::relay::machine_session;
use super::{plan, CrewHub};
use crate::serve::AgentService;

/// The grant shape the machines read.
const GRANT_VERSION: u32 = workforce_desk::grant::GRANT_VERSION;

/// One grant a machine should hold.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Wanted {
    pub group: String,
    /// The boss, as this hub's agent id and as the person knows it.
    pub agent: String,
    pub agent_name: String,
    /// Which making of the agent this is ([`crate::agent_generation`]), so
    /// an agent made again with the same id isn't taken for the one the
    /// person said yes to. None where this hub can't keep it.
    pub agent_since: Option<String>,
    /// The plan whose ticket sessions it holds, when it is a plan's
    /// ([`plan_key`]): the person approved it, so Desk asks nothing more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<Uuid>,
    /// What `PUT /api/desk/grants/{group}` says, but its revision and the
    /// sessions leaving it, which only the machine's copy can tell.
    pub grant: Value,
}

/// What one machine should hold.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineWanted {
    pub grants: Vec<Wanted>,
    /// Every one of its sessions the sidebar lists, in any group or none.
    pub listed: BTreeSet<Uuid>,
}

/// The grants every machine should hold, by machine.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineGrants {
    pub grant_version: u32,
    pub machines: BTreeMap<String, MachineWanted>,
}

/// A boss's own `[oversight.act]` switches that a grant carries: those a
/// served boss's are read from (`served_oversight`), each as it is here.
fn act_of(o: &Oversight) -> Value {
    let a = &o.act;
    json!({
        "nudge": a.nudge,
        "resume_restart": a.resume_restart,
        "read_only_approvals": a.read_only_approvals,
        "hand_out_work": a.hand_out_work,
        "interrupt": a.interrupt,
        "answer_sessions": a.answer_sessions,
        "parallel": o.work.parallel,
    })
}

/// `act` with Act fully's daily budget for a machine: the boss's own, or
/// the most Act fully allows when it set none.
fn with_budget(mut act: Value, o: &Oversight) -> Value {
    let usd = o
        .full
        .budget_usd_per_day()
        .unwrap_or(workforce_common::oversight::MAX_FULL_BUDGET_USD);
    act["budget_usd_per_day"] = json!(usd);
    act
}

/// What a plan grant's id starts with.
const PLAN_KEY: &str = "plan-";

/// The id of plan `plan`'s grant on a machine.
pub(crate) fn plan_key(plan: Uuid) -> String {
    format!("{PLAN_KEY}{plan}")
}

/// The plan `group` is the grant of, when it is a plan's.
pub(crate) fn plan_of(group: &str) -> Option<Uuid> {
    group
        .strip_prefix(PLAN_KEY)?
        .parse()
        .ok()
        .filter(|p| plan_key(*p) == group)
}

/// `name` as a grant names its group, or `or` when it is blank.
fn named(name: &str, or: &str) -> String {
    match name.trim() {
        "" => or.to_string(),
        n => n
            .char_indices()
            .take_while(|(i, c)| i + c.len_utf8() <= 200)
            .map(|(_, c)| c)
            .collect(),
    }
}

/// What every grant for one boss says of it.
struct Granted {
    boss: BossRef,
    agent: String,
    agent_name: String,
    since: Option<String>,
    level: Level,
    act: Value,
}

impl Granted {
    /// The grant of `members` in `group`, called `name`.
    fn wanted(
        &self,
        group: String,
        name: String,
        members: BTreeSet<Uuid>,
        plan: Option<Uuid>,
    ) -> Wanted {
        let members: Vec<Uuid> = members.into_iter().take(MAX_MEMBERS).collect();
        Wanted {
            group,
            agent: self.agent.clone(),
            agent_name: self.agent_name.clone(),
            agent_since: self.since.clone(),
            plan,
            grant: json!({
                "grantVersion": GRANT_VERSION,
                "name": name,
                "boss": self.boss.key(),
                "level": self.level.as_str(),
                "sees": workforce_desk::grant::Sees::Everything.as_str(),
                "recall": false,
                "folders": [],
                "repos": [],
                "members": members,
                "act": self.act,
            }),
        }
    }
}

/// Each plan's ticket sessions on each machine, by machine and plan, with
/// the plan's goal.
pub(crate) type PlanSessions = BTreeMap<(String, Uuid), (String, BTreeSet<Uuid>)>;

impl CrewHub {
    /// What `service`'s grants say of it, as the module says, at Act fully
    /// only where `fully`; none for an id a grant can't name.
    fn granted(&self, service: &AgentService, o: &Oversight, fully: bool) -> Option<Granted> {
        let agent = service.agent_id.as_str();
        let boss = BossRef::Local {
            agent: agent.to_string(),
        };
        if BossRef::parse(&boss.key()).as_ref() != Ok(&boss) {
            return None;
        }
        // Act fully as chosen, not as in force this moment: whether it is
        // in force is said to each machine every second, with the one
        // budget, so a spent budget, midnight or a restart doesn't
        // rewrite the grant or ask the person again.
        let level = match o.level {
            Level::Full if o.fully().is_ok() => Level::Full,
            _ => self.level_now(agent, o).0,
        };
        let level = match level {
            Level::Full if !fully => Level::Act,
            l => l,
        };
        let act = match level {
            Level::Full => with_budget(act_of(o), o),
            _ => act_of(o),
        };
        Some(Granted {
            since: crate::agent_generation::of(
                std::path::Path::new(&service.config.hub.data_dir),
                agent,
            ),
            agent_name: super::name_of(service),
            agent: agent.to_string(),
            boss,
            level,
            act,
        })
    }

    /// The ticket sessions `agent`'s plans run on the person's machines, as
    /// the plan runner last read them: only for a boss of no group (a group
    /// boss's are its group's), only while it sees each in the sidebar
    /// (never one in a group with a boss of its own), and none of a plan
    /// whose key a sidebar group has.
    pub(crate) fn plan_sessions(&self, agent: &str, o: &Oversight) -> PlanSessions {
        if o.scope != Scope::Ungrouped {
            return PlanSessions::new();
        }
        let ours: Vec<(String, plan::MachineSeen)> = self
            .machine_seen
            .lock()
            .iter()
            .filter(|(_, s)| s.boss == agent && s.state != "gone")
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        let seen = self.desk.seen_by(&o.scope);
        ours.into_iter()
            .filter_map(|(item, s)| {
                let (m, id) = machine_session(&item)?;
                let group = self.desk.group_of_member(&format!("session:{item}"));
                (seen.sees(group.as_deref()) && !self.desk.is_group(&plan_key(s.plan)))
                    .then(|| ((m.to_string(), s.plan), s.goal, id))
            })
            .fold(PlanSessions::new(), |mut out, (k, goal, id)| {
                out.entry(k)
                    .or_insert_with(|| (goal, BTreeSet::new()))
                    .1
                    .insert(id);
                out
            })
    }

    /// The grant `agent`'s call on session `id` of `machine` goes under
    /// when it is one of its own plans' ticket sessions there.
    pub(crate) fn plan_grant_of(
        &self,
        agent: &str,
        o: &Oversight,
        machine: &str,
        id: Uuid,
    ) -> Option<String> {
        self.plan_sessions(agent, o)
            .into_iter()
            .find(|((m, _), (_, members))| m == machine && members.contains(&id))
            .map(|((_, plan), _)| plan_key(plan))
    }

    /// The grants each machine should hold for this hub's group bosses and
    /// the main overseer's plans, as the module says; none while this hub
    /// hasn't read its agents, who runs which session, or its plans'
    /// machine sessions yet, so nothing is taken off a machine for what
    /// isn't known.
    pub(crate) fn machine_grants(&self) -> Option<MachineGrants> {
        if self.services().is_empty()
            || !self.desk.charges_loaded()
            || !self
                .plans_watched
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        let layout = self.desk.layout();
        let mut out = MachineGrants {
            grant_version: GRANT_VERSION,
            machines: BTreeMap::new(),
        };
        let all = layout
            .groups
            .iter()
            .flat_map(|g| g.items.iter())
            .chain(layout.ungrouped.items.iter());
        for item in all {
            if let Some((m, id)) = item.strip_prefix("session:").and_then(machine_session) {
                out.machines
                    .entry(m.to_string())
                    .or_default()
                    .listed
                    .insert(id);
            }
        }
        // A group whose boss is on another hub is that boss's here.
        let remote: HashSet<String> = self
            .desk
            .grants_bossed()
            .into_iter()
            .filter(|(g, _)| matches!(g.boss, Some(BossRef::Remote { .. })))
            .map(|(g, _)| g.group)
            .collect();
        let overseers = self.overseers();
        let mut add = |m: &str, w: Wanted| {
            out.machines
                .entry(m.to_string())
                .or_default()
                .grants
                .push(w);
        };
        for g in &layout.groups {
            if remote.contains(&g.id)
                || !workforce_desk::grant::group_id_ok(&g.id)
                || plan_of(&g.id).is_some()
            {
                continue;
            }
            let mut by_machine: BTreeMap<&str, BTreeSet<Uuid>> = BTreeMap::new();
            for item in &g.items {
                if let Some((m, id)) = item.strip_prefix("session:").and_then(machine_session) {
                    by_machine.entry(m).or_default().insert(id);
                }
            }
            let scope = Scope::Group(g.id.clone());
            let Some(granted) = (!by_machine.is_empty())
                .then(|| overseers.iter().find(|(_, o)| o.scope == scope))
                .flatten()
                .and_then(|(service, o)| self.granted(service, o, true))
            else {
                continue;
            };
            let name = named(&g.name, &g.id);
            for (m, members) in by_machine {
                add(m, granted.wanted(g.id.clone(), name.clone(), members, None));
            }
        }
        for (service, o) in &overseers {
            let plans = self.plan_sessions(service.agent_id.as_str(), o);
            let Some(granted) = (!plans.is_empty())
                .then(|| self.granted(service, o, false))
                .flatten()
            else {
                continue;
            };
            for ((m, plan), (goal, members)) in plans {
                add(
                    &m,
                    granted.wanted(plan_key(plan), named(&goal, "Plan"), members, Some(plan)),
                );
            }
        }
        Some(out)
    }
}
