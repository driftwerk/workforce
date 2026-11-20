//! The grants each of the person's machines should hold for this hub's own
//! group bosses (remote bosses §4.3): a local boss reaches its group's
//! machine sessions through the relay, and the machine decides each call
//! under its grant for the group. This hub works out what each grant says,
//! from its own state alone; Desk's native side only carries it to each
//! machine (`GET /api/desk/machine-grants`, the hub's own token from the
//! `Authorization` header alone, as the relay's routes are; `?plans=1`
//! from a Desk that carries plans' grants, which an older one would ask
//! the person about as a group's).
//!
//! Same rules, no wider powers: a grant says exactly what the boss may do
//! here, in the same group, and nothing more.
//!
//! - **Who:** each group's own boss ([`CrewHub::overseers`]), as
//!   `local:<agent>`. A group with no boss of its own, Ungrouped, a group
//!   named for a boss that oversees nothing now, and a group this hub
//!   holds a grant for whose boss is on another hub get none: one boss per
//!   group.
//! - **The main overseer's plans:** it runs no group, so each of its running
//!   plans gets a grant of its own on each machine its tickets work on
//!   ([`plan_key`], never a sidebar group's id), from before the first of
//!   them starts there: each starts in it, run by the boss from launch. It
//!   holds exactly the plan's ticket sessions there as the plan runner last
//!   read them, while the boss sees them in the sidebar, and goes once the
//!   plan has none there and nothing left to start there, or ends.
//! - **What it may do:** its level as chosen here (Act fully as chosen,
//!   the rest as it runs now, [`CrewHub::level_now`]), and its own `[oversight.act]` switches and
//!   `parallel`. At Act fully the grant carries its daily budget (with none
//!   set here, the most Act fully allows), and that budget is one across
//!   this computer and every grant of the boss on every machine
//!   ([`super::shared_budget`]): each machine acts fully only while this
//!   hub's take, at most [`super::relayed_decide::FRESH`] old, says so with
//!   what was spent everywhere else.
//! - **What it sees:** everything, terminals included, as its
//!   `crew.history` here shows them; the machine still redacts and caps
//!   every string.
//! - **Never more:** no folders (its boss starts no session there; a plan's
//!   ticket starts in its plan's grant only as the plan runner starts it),
//!   no repositories (Act fully ships nowhere from a grant), no memory
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
use workforce_desk::grant::{plan_key, plan_of, BossRef, MAX_MEMBERS};

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
    /// Whether its plans' grants are said: false until its plans' machine
    /// sessions have been read, or for a Desk that doesn't carry them.
    pub plans_known: bool,
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
/// the plan's goal: none yet where its tickets are still to start.
pub(crate) type PlanSessions = BTreeMap<(String, Uuid), (String, BTreeSet<Uuid>)>;

impl CrewHub {
    /// What `service`'s grants say of it, as the module says; none for an
    /// id a grant can't name.
    fn granted(&self, service: &AgentService, o: &Oversight) -> Option<Granted> {
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
    /// the plan runner last read them, and the machines its running plans'
    /// tickets are still to start on: only for a boss of no group (a group
    /// boss's are its group's), only while it sees each session in the
    /// sidebar (never one in a group with a boss of its own), and none of
    /// a plan whose key a sidebar group has.
    pub(crate) fn plan_sessions(&self, agent: &str, o: &Oversight) -> PlanSessions {
        if o.scope != Scope::Ungrouped {
            return PlanSessions::new();
        }
        let ours = |plan: Uuid| !self.desk.is_group(&plan_key(plan));
        let to_start: PlanSessions = self
            .machine_plans
            .lock()
            .iter()
            .filter(|((_, plan), p)| p.boss == agent && ours(*plan))
            .map(|(k, p)| (k.clone(), (p.goal.clone(), BTreeSet::new())))
            .collect();
        let seen: Vec<(String, plan::MachineSeen)> = self
            .machine_seen
            .lock()
            .iter()
            .filter(|(_, s)| {
                s.boss == agent
                    && s.state != "gone"
                    && s.engine.is_some_and(workforce_desk::engine::boss_safe)
            })
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        let sees = self.desk.seen_by(&o.scope);
        seen.into_iter()
            .filter_map(|(item, s)| {
                let (m, id) = machine_session(&item)?;
                let group = self.desk.group_of_member(&format!("session:{item}"));
                (sees.sees(group.as_deref()) && ours(s.plan))
                    .then(|| ((m.to_string(), s.plan), s.goal, id))
            })
            .fold(to_start, |mut out, (k, goal, id)| {
                out.entry(k)
                    .or_insert_with(|| (goal, BTreeSet::new()))
                    .1
                    .insert(id);
                out
            })
    }

    /// Each machine whose sessions `agent` runs, with the grant they are
    /// under there: its group's, by the sidebar, or each of its plans'.
    pub(crate) fn machine_grants_of(&self, agent: &str, o: &Oversight) -> Vec<(String, String)> {
        match o.scope.group() {
            Some(g) if plan_of(g).is_some() => Vec::new(),
            Some(g) => self
                .machines_in(g)
                .into_iter()
                .map(|m| (m, g.to_string()))
                .collect(),
            None => self
                .plan_sessions(agent, o)
                .into_keys()
                .map(|(m, plan)| (m, plan_key(plan)))
                .collect(),
        }
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

    /// The grants each machine should hold for this hub's group bosses and,
    /// for a Desk that carries them (`plans`), the main overseer's plans, as
    /// the module says; none while this hub hasn't read its agents or who
    /// runs which session, so nothing is taken off a machine for what isn't
    /// known. Its plans' grants are said once its plans' machine sessions
    /// have been read (`plansKnown`): until then Desk leaves them as they
    /// are, and the groups' go on.
    pub(crate) fn machine_grants(&self, plans: bool) -> Option<MachineGrants> {
        if self.services().is_empty() || !self.desk.charges_loaded() {
            return None;
        }
        let plans = plans
            && self
                .plans_watched
                .load(std::sync::atomic::Ordering::Acquire);
        let layout = self.desk.layout();
        let mut out = MachineGrants {
            grant_version: GRANT_VERSION,
            plans_known: plans,
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
                .and_then(|(service, o)| self.granted(service, o))
            else {
                continue;
            };
            let name = named(&g.name, &g.id);
            for (m, members) in by_machine {
                add(m, granted.wanted(g.id.clone(), name.clone(), members, None));
            }
        }
        for (service, o) in overseers.iter().filter(|_| plans) {
            let plans = self.plan_sessions(service.agent_id.as_str(), o);
            let Some(granted) = (!plans.is_empty())
                .then(|| self.granted(service, o))
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
