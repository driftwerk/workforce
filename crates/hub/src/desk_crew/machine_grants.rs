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
//!   group, and the main overseer's calls aren't relayed yet.
//! - **What it may do:** its level as it runs here now
//!   ([`CrewHub::level_now`]: Act fully only while it is in force), and
//!   its own `[oversight.act]` switches and `parallel`. At Act fully its
//!   daily budget too, when it has one; with none, the machine acts within
//!   Desk (`FULL_NO_BUDGET`), which is less.
//! - **What it sees:** everything, terminals included, as its
//!   `crew.history` here shows them; the machine still redacts and caps
//!   every string.
//! - **Never more:** no folders (it starts no session there), no
//!   repositories (Act fully ships nowhere from a grant), no memory
//!   search, never a resume of a stop.
//! - **Which sessions:** that machine's sessions the sidebar has in the
//!   group, and no other.
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
use super::CrewHub;

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
fn act_of(o: &Oversight, level: Level) -> Value {
    let a = &o.act;
    let mut act = json!({
        "nudge": a.nudge,
        "resume_restart": a.resume_restart,
        "read_only_approvals": a.read_only_approvals,
        "hand_out_work": a.hand_out_work,
        "interrupt": a.interrupt,
        "answer_sessions": a.answer_sessions,
        "parallel": o.work.parallel,
    });
    if level == Level::Full {
        if let Some(b) = o.full.budget_usd_per_day() {
            act["budget_usd_per_day"] = json!(b);
        }
    }
    act
}

impl CrewHub {
    /// The grants each machine should hold for this hub's group bosses, as
    /// the module says.
    pub(crate) fn machine_grants(&self) -> MachineGrants {
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
        for g in &layout.groups {
            if remote.contains(&g.id) || !workforce_desk::grant::group_id_ok(&g.id) {
                continue;
            }
            let scope = Scope::Group(g.id.clone());
            let Some((service, o)) = overseers.iter().find(|(_, o)| o.scope == scope) else {
                continue;
            };
            let agent = service.agent_id.as_str();
            let boss = BossRef::Local {
                agent: agent.to_string(),
            };
            // An id a grant can't name gets none.
            if BossRef::parse(&boss.key()).as_ref() != Ok(&boss) {
                continue;
            }
            let mut by_machine: BTreeMap<&str, BTreeSet<Uuid>> = BTreeMap::new();
            for item in &g.items {
                if let Some((m, id)) = item.strip_prefix("session:").and_then(machine_session) {
                    by_machine.entry(m).or_default().insert(id);
                }
            }
            if by_machine.is_empty() {
                continue;
            }
            let (level, _) = self.level_now(agent, o);
            let name: String = match g.name.trim() {
                "" => g.id.clone(),
                n => n
                    .char_indices()
                    .take_while(|(i, c)| i + c.len_utf8() <= 200)
                    .map(|(_, c)| c)
                    .collect(),
            };
            for (m, members) in by_machine {
                let members: Vec<Uuid> = members.into_iter().take(MAX_MEMBERS).collect();
                let grant = json!({
                    "grantVersion": GRANT_VERSION,
                    "name": name,
                    "boss": boss.key(),
                    "level": level.as_str(),
                    "sees": workforce_desk::grant::Sees::Everything.as_str(),
                    "recall": false,
                    "folders": [],
                    "repos": [],
                    "members": members,
                    "act": act_of(o, level),
                });
                out.machines
                    .entry(m.to_string())
                    .or_default()
                    .grants
                    .push(Wanted {
                        group: g.id.clone(),
                        agent: agent.to_string(),
                        agent_name: super::name_of(service),
                        grant,
                    });
            }
        }
        out
    }
}
