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
use workforce_common::oversight::{Level, Oversight, Scope, TakeOn};
use workforce_desk::crew::{Crew, MachineSession};
use workforce_desk::grant::{plan_key, plan_of, BossRef, MAX_MEMBERS};
use workforce_desk::UNGROUPED;

use super::machines::ANY_MACHINE;
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
    /// Whether its Ungrouped grant is said: false until its sessions have
    /// been read since Desk began carrying Ungrouped grants, and until then
    /// Desk leaves the one it holds as it is.
    pub ungrouped_known: bool,
}

/// Desk's asks for machines' Ungrouped grants.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Asked {
    /// The first of the asks it makes now, none lapsing between.
    since: std::time::Instant,
    last: std::time::Instant,
}

/// The grants every machine should hold, by machine.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineGrants {
    pub grant_version: u32,
    /// Whether its plans' grants are said: false until its plans' machine
    /// sessions have been read, or for a Desk that doesn't carry them.
    pub plans_known: bool,
    /// Whether every machine's Ungrouped grant is said, listed here or not:
    /// with no main overseer, none should hold one.
    pub ungrouped_known: bool,
    pub machines: BTreeMap<String, MachineWanted>,
}

/// A boss's own `[oversight.act]` switches that a grant carries: those a
/// served boss's are read from (`served_oversight`), each as it is here.
fn act_of(o: &Oversight) -> Value {
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
    act[super::served::TAKE_ON_ALL] = json!(o.work.take_on == TakeOn::All);
    act[super::served::HOLD_MINUTES] = json!(o.wake.hold_minutes);
    act[super::served::ANSWER_AFTER_MINUTES] = json!(o.wake.answer_after_minutes);
    act
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

/// The machine and session a sidebar item names, when it is a machine's.
fn machine_item(item: &str) -> Option<(String, Uuid)> {
    item.strip_prefix("session:")
        .and_then(machine_session)
        .map(|(m, id)| (m.to_string(), id))
}

/// Sessions by the machine they are on.
pub(crate) type MachineSessions = BTreeMap<String, BTreeSet<Uuid>>;

/// How long Desk's last ask for machines' Ungrouped grants says it carries
/// them: well past its polls, which come every 30 seconds.
const UNGROUPED_ASKED_FOR: std::time::Duration = std::time::Duration::from_secs(300);

/// Each plan's ticket sessions on each machine, by machine and plan, with
/// the plan's goal: none yet where its tickets are still to start.
pub(crate) type PlanSessions = BTreeMap<(String, Uuid), (String, BTreeSet<Uuid>)>;

/// A machine's open sessions, as Desk there last listed them, and when.
pub(crate) struct Listed {
    at: std::time::Instant,
    /// The listing says which grant holds each session there. One that
    /// doesn't (an older Desk) is as if not read: a session another grant
    /// holds there can't be told apart, so none is written Ungrouped.
    grants_said: bool,
    open: BTreeSet<Uuid>,
}

impl Listed {
    /// Whether it says what Ungrouped can hold there, as read at or after
    /// `since`.
    fn read_since(&self, since: std::time::Instant) -> bool {
        self.grants_said && self.at >= since
    }
}

/// How long one machine's sessions are waited for, as the Ungrouped grants
/// are read.
const MACHINE_READ_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a machine Desk reaches may stay unread, Desk carrying its
/// Ungrouped grant, before it is said: well past a few reads.
const UNREAD_SAID_AFTER: std::time::Duration = std::time::Duration::from_secs(120);

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

    /// Each machine's sessions the sidebar has in Ungrouped that the main
    /// overseer `o` reaches by its machine's Ungrouped grant: none for
    /// a group's boss, and none of its plans' (a machine holds a session in
    /// one grant only). As the sidebar resolves it: each machine's open
    /// sessions, as last read there, in no group; none of a machine not
    /// read yet, or whose listing doesn't say which grant holds each, and
    /// none while Desk doesn't carry machines' Ungrouped grants.
    pub(crate) fn ungrouped_sessions(&self, o: &Oversight) -> MachineSessions {
        let Some(since) = self.ungrouped_since() else {
            return MachineSessions::new();
        };
        if o.scope != Scope::Ungrouped {
            return MachineSessions::new();
        }
        // Every plan's session there, whatever its engine or state: a plan's
        // grant has it, never this one.
        let planned: BTreeSet<(String, Uuid)> = self
            .machine_seen
            .lock()
            .keys()
            .filter_map(|item| machine_session(item))
            .map(|(m, id)| (m.to_string(), id))
            .collect();
        let grouped = self.machine_grouped();
        self.machine_known
            .lock()
            .iter()
            .filter(|(_, l)| l.read_since(since))
            .flat_map(|(m, l)| l.open.iter().map(move |id| (m.clone(), *id)))
            .filter(|k| !grouped.contains(k) && !planned.contains(k))
            .fold(MachineSessions::new(), |mut out, (m, id)| {
                out.entry(m).or_default().insert(id);
                out
            })
    }

    /// Each machine session the sidebar has in a group.
    fn machine_grouped(&self) -> BTreeSet<(String, Uuid)> {
        self.desk
            .layout()
            .groups
            .iter()
            .flat_map(|g| g.items.iter())
            .filter_map(|i| machine_item(i))
            .collect()
    }

    /// How far apart one machine's sessions are read.
    pub(super) fn machine_gap(&self) -> std::time::Duration {
        self.timing.tick.min(std::time::Duration::from_secs(15))
    }

    /// Keeps `machine`'s sessions as Desk there just `listed` them, but
    /// those closed, and gives them back read.
    pub(super) fn machine_listed(
        &self,
        machine: &str,
        listed: Value,
    ) -> Option<Vec<MachineSession>> {
        let grants_said = listed
            .as_array()
            .is_some_and(|l| l.iter().all(|s| s.get("grant").is_some()));
        let listed: Vec<MachineSession> = match serde_json::from_value(listed) {
            Ok(l) => l,
            Err(e) => {
                tracing::debug!(%machine, error = %e, "a machine's sessions couldn't be read");
                return None;
            }
        };
        // One another grant there holds (another home's, or a group's) is
        // never asked for: the machine would refuse the whole grant.
        let open = listed
            .iter()
            .filter(|s| s.state != "closed")
            .filter(|s| s.grant.as_deref().is_none_or(|g| g == UNGROUPED))
            .map(|s| s.id)
            .collect();
        let l = Listed {
            at: std::time::Instant::now(),
            grants_said,
            open,
        };
        self.machine_known.lock().insert(machine.to_string(), l);
        Some(listed)
    }

    /// Reads the sessions of each of the person's machines Desk reaches, at
    /// most [`Self::machine_gap`] apart, while Desk carries machines'
    /// Ungrouped grants: what [`Self::ungrouped_sessions`] are made of. A
    /// machine Desk no longer has is forgotten; one that can't be read
    /// keeps what was last read of it.
    pub(super) async fn watch_ungrouped(&self) {
        let main = self
            .overseers()
            .into_iter()
            .find(|(_, o)| o.scope == Scope::Ungrouped)
            .map(|(s, _)| s.agent_id.as_str().to_string());
        // What was read is kept while Desk doesn't ask: it counts again only
        // as read after Desk asks again.
        let Some((boss, since)) = main.zip(self.ungrouped_since()) else {
            return;
        };
        let gap = self.machine_gap();
        {
            let mut read = self.machine_read.lock();
            if read.get(ANY_MACHINE).is_some_and(|at| at.elapsed() < gap) {
                return;
            }
            read.insert(ANY_MACHINE.to_string(), std::time::Instant::now());
        }
        let machines = match self
            .machine_ask(ANY_MACHINE, &boss, "desk.machines", json!({}))
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "the person's machines weren't read");
                return;
            }
        };
        let had: BTreeSet<String> = machines["machines"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["name"].as_str())
            .filter(|m| super::relay::machine_ok(m))
            .map(str::to_string)
            .collect();
        self.machine_known.lock().retain(|m, _| had.contains(m));
        let reached: Vec<&str> = machines["machines"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["reached"].as_bool() == Some(true))
            .filter_map(|m| m["name"].as_str())
            .filter(|m| had.contains(*m))
            .collect();
        // Each at once, none waited on past a short while: a slow machine
        // keeps what was last read of it.
        let due = reached.iter().copied().filter(|m| {
            self.machine_known
                .lock()
                .get(*m)
                .is_none_or(|l| l.at.elapsed() >= gap)
        });
        let read = due.map(|machine| {
            let boss = &boss;
            async move {
                let listed = tokio::time::timeout(
                    MACHINE_READ_WITHIN,
                    self.machine_call(machine, None, boss, "desk.sessions", json!({})),
                )
                .await;
                (machine, listed)
            }
        });
        for (machine, listed) in futures::future::join_all(read).await {
            match listed {
                Ok(Ok(v)) => {
                    self.machine_listed(machine, v);
                }
                Ok(Err(e)) => {
                    tracing::debug!(%machine, error = %e, "a machine's sessions weren't read")
                }
                Err(_) => tracing::debug!(%machine, "a machine's sessions weren't read in time"),
            }
        }
        self.say_unread(&reached, since);
    }

    /// Says once, in the log and on the Floor, each machine Desk reaches
    /// whose sessions stay unread past [`UNREAD_SAID_AFTER`] of carrying its
    /// Ungrouped grant: the main overseer runs none of its sessions in no
    /// group, so their questions are the person's.
    fn say_unread(&self, reached: &[&str], since: std::time::Instant) {
        let unread: BTreeSet<String> = {
            let known = self.machine_known.lock();
            reached
                .iter()
                .filter(|m| !known.get(**m).is_some_and(|l| l.read_since(since)))
                .map(|m| m.to_string())
                .collect()
        };
        let said: Vec<String> = {
            let mut said = self.unread_said.lock();
            said.retain(|m| unread.contains(m));
            match since.elapsed() >= UNREAD_SAID_AFTER {
                true => unread
                    .into_iter()
                    .filter(|m| said.insert(m.clone()))
                    .collect(),
                false => Vec::new(),
            }
        };
        for machine in said {
            tracing::warn!(%machine, "a machine's sessions stay unread: its Ungrouped grant isn't said");
            self.desk.note(workforce_desk::crew::Event::new(
                workforce_desk::crew::Kind::OnMachine,
                workforce_desk::crew::Party::desk(),
                None,
                format!(
                    "Desk can't read the sessions on {machine}, so the main boss doesn't run those \
                     in no group there and their questions come to you. Updating Desk on {machine} \
                     may fix it."
                ),
            ));
        }
    }

    /// As if Desk began carrying machines' Ungrouped grants long ago.
    #[cfg(test)]
    pub(crate) fn ungrouped_carried_long(&self) {
        if let Some(a) = self.ungrouped_asked.lock().as_mut() {
            a.since -= UNREAD_SAID_AFTER;
        }
        self.machine_read.lock().clear();
    }

    /// Whether Desk on this computer carries machines' Ungrouped grants:
    /// it asked for them lately.
    pub(crate) fn ungrouped_carried(&self) -> bool {
        self.ungrouped_since().is_some()
    }

    /// As if Desk last asked for machines' Ungrouped grants long ago.
    #[cfg(test)]
    pub(crate) fn ungrouped_lapsed(&self) {
        if let Some(a) = self.ungrouped_asked.lock().as_mut() {
            a.last -= UNGROUPED_ASKED_FOR;
        }
    }

    /// Since when Desk has carried machines' Ungrouped grants, while it
    /// does.
    fn ungrouped_since(&self) -> Option<std::time::Instant> {
        self.ungrouped_asked
            .lock()
            .filter(|a| a.last.elapsed() < UNGROUPED_ASKED_FOR)
            .map(|a| a.since)
    }

    /// The grant the main overseer's (`o`'s) call on session `id` of
    /// `machine` goes under when it is one of that machine's sessions in
    /// Ungrouped, or why it can't be reached.
    pub(crate) fn ungrouped_grant_of(
        &self,
        o: &Oversight,
        machine: &str,
        id: Uuid,
    ) -> Result<String, String> {
        let ours = self
            .ungrouped_sessions(o)
            .get(machine)
            .is_some_and(|ids| ids.contains(&id));
        // A Desk that doesn't carry them: one the sidebar has in no group
        // can't be reached, as Desk there isn't read for it.
        let older = !self.ungrouped_carried()
            && o.scope == Scope::Ungrouped
            && !self.machine_grouped().contains(&(machine.to_string(), id));
        match (ours, older) {
            (true, _) => Ok(UNGROUPED.to_string()),
            (false, true) => Err(format!(
                "Desk on this computer is older and can't let you reach {machine}'s ungrouped sessions: tell the person to update Desk."
            )),
            (false, false) => Err(format!(
                "On {machine} you run only your own plans' ticket sessions and the sessions in no group, and this isn't one of them."
            )),
        }
    }

    /// Each machine whose sessions `agent` runs, with the grant they are
    /// under there: its group's, by the sidebar; or each of its plans', and
    /// the machine's Ungrouped grant while Desk carries it.
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
                .chain(
                    self.ungrouped_sessions(o)
                        .into_keys()
                        .filter(|_| self.ungrouped_carried())
                        .map(|m| (m, UNGROUPED.to_string())),
                )
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
    pub(crate) fn machine_grants(&self, plans: bool, ungrouped: bool) -> Option<MachineGrants> {
        if ungrouped {
            let now = std::time::Instant::now();
            let since = self.ungrouped_since().unwrap_or(now);
            *self.ungrouped_asked.lock() = Some(Asked { since, last: now });
        }
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
            ungrouped_known: false,
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
        // Said only once the plans' sessions are known, so none of theirs is
        // taken into it.
        for (service, o) in overseers.iter().filter(|_| plans && ungrouped) {
            let machines = self.ungrouped_sessions(o);
            let Some(granted) = (!machines.is_empty())
                .then(|| self.granted(service, o))
                .flatten()
            else {
                continue;
            };
            for (m, members) in machines {
                add(
                    &m,
                    granted.wanted(UNGROUPED.to_string(), "Ungrouped".into(), members, None),
                );
            }
        }
        // Each machine read since Desk began asking: what it should hold in
        // Ungrouped is said, none included. With no main overseer, every
        // machine's is: none.
        if plans && ungrouped {
            out.ungrouped_known = !overseers.iter().any(|(_, o)| o.scope == Scope::Ungrouped);
            let since = self.ungrouped_since().filter(|_| !out.ungrouped_known);
            let known: Vec<String> = self
                .machine_known
                .lock()
                .iter()
                .filter(|(_, l)| since.is_some_and(|s| l.read_since(s)))
                .map(|(m, _)| m.clone())
                .collect();
            for m in known {
                out.machines.entry(m).or_default().ungrouped_known = true;
            }
        }
        Some(out)
    }
}
