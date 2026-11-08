//! The crew manager's hub side: who oversees the crew (`[oversight]`, one
//! overseer per scope: the main one over everything ungrouped, and a boss
//! for each sidebar group that has one, each seeing only its own scope),
//! what the hub's agents are doing (recorded from their turns as
//! activity), the overseer's `crew.*` tools and its suggestions, and its
//! wake-ups and summaries, run as turns in its Desk DM with the person.
//!
//! The activity log and the overview live in `workforce-desk`; this is the
//! [`Crew`] the desk asks about agents, the overseer and suggestions.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use workforce_common::channels::DESK_CHANNEL;
use workforce_common::oversight::{Oversight, Scope};
use workforce_common::settings::AgentWorkspaceConfig;
use workforce_desk::crew::{By, Crew, CrewAgent, LiveTurn, Overseer, Party, Suggestion};
use workforce_desk::floor::Need;
use workforce_desk::Desk;

use crate::gateway::GatewayState;
use crate::serve::AgentService;

mod actions;
mod board;
mod deliver;
mod extras;
mod line;
mod observe;
mod plan;
mod read_only;
mod reading;
mod scope;
mod spend;
mod tools;
mod watcher;
mod work;

#[cfg(test)]
pub(crate) mod tests;

pub(crate) use extras::mentions_for;
pub use observe::Observed;
pub use plan::{plan_channel, PLAN};

/// What a check-in's prompt starts with: the person never typed it, so the
/// DM's history leaves it out.
pub const CHECK_IN: &str = "[Desk check-in]";

/// Hand-offs between agents allowed from one thing that happened, as in
/// rooms.
pub const MAX_HOPS: i16 = workforce_desk::rooms::MAX_HOPS;
/// Messages the overseer may send other agents before the person speaks
/// again, as in rooms.
pub const TURN_BUDGET: usize = workforce_desk::rooms::TURN_BUDGET as usize;

/// How often the watcher looks, how long it gathers what happened into one
/// wake-up, and how long a "minute" of `[oversight.wake]` is (shorter in
/// tests).
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub tick: Duration,
    pub coalesce: Duration,
    pub minute: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(30),
            coalesce: Duration::from_secs(60),
            minute: Duration::from_secs(60),
        }
    }
}

/// A turn an agent is running now.
#[derive(Debug, Clone)]
struct Running {
    agent_id: String,
    task: Option<String>,
    detail: Option<String>,
    since: DateTime<Utc>,
    last_activity: DateTime<Utc>,
}

pub struct CrewHub {
    me: Weak<CrewHub>,
    state: Arc<GatewayState>,
    desk: Desk,
    timing: Timing,
    /// Agents' turns running now, by session.
    running: Mutex<HashMap<String, Running>>,
    /// The hop count a turn about to start in a session carries.
    hops: Mutex<HashMap<String, i16>>,
    /// The hop of the wake-up each overseer is answering, while it is, by
    /// its agent.
    wake_hop: Mutex<HashMap<String, i16>>,
    /// The sessions whose boss, still an agent here, has read as no
    /// overseer on every look since: how many looks, and since when. One is
    /// let go only once that has lasted ([`work::LET_GO_AFTER_LOOKS`]), never
    /// on a config that failed to read once.
    unbossed: Mutex<HashMap<uuid::Uuid, (u32, std::time::Instant)>>,
    /// When the overseer's turns in its DM started, by session.
    manager_turns: Mutex<HashMap<String, DateTime<Utc>>>,
    /// Pending approvals a wake-up listed, by the overseer's DM session.
    listed: Mutex<HashMap<String, Vec<workforce_desk::crew::MentionedApproval>>>,
    /// When the overseer messaged other agents since the person last spoke.
    sent: Mutex<Vec<DateTime<Utc>>>,
    /// Each agent's `[oversight]`, by the config.toml mtime it was read at.
    read: Mutex<HashMap<String, (Option<SystemTime>, Oversight)>>,
    /// The overseers the tools were last set up for, by agent: each one's
    /// scope and level.
    tooled: Mutex<HashMap<String, (Scope, String)>>,
    /// Ask the sync loop to look again now (a config was saved).
    poke: tokio::sync::Notify,
    /// The words of sessions' questions on Desk's line the overseer may
    /// answer, by message: an answer is gated by what was asked as well as
    /// by what it says. One not known here is the person's.
    questions: Mutex<HashMap<uuid::Uuid, String>>,
    /// The work turns each overseer spent on its sessions' turn ends, by
    /// its agent.
    work: Mutex<HashMap<String, work::WorkRun>>,
    /// Held while the overseer starts a session: counting its busy
    /// sessions and starting one more are one step.
    starting: tokio::sync::Mutex<()>,
    /// The plan tickets whose session failed to start: each is tried again
    /// after a wait that doubles.
    plan_failed: Mutex<HashMap<uuid::Uuid, plan::Failed>>,
    /// What failed to start, for its plan's next turn.
    plan_failures: Mutex<Vec<(uuid::Uuid, String)>>,
}

static CREWS: Mutex<Vec<Weak<CrewHub>>> = Mutex::new(Vec::new());

/// The crew `service` belongs to, if the desk is on for its hub.
pub(crate) fn for_service(service: &AgentService) -> Option<Arc<CrewHub>> {
    CREWS.lock().iter().filter_map(Weak::upgrade).find(|c| {
        c.state
            .registry
            .get(service.agent_id.as_str())
            .is_some_and(|s| std::ptr::eq(Arc::as_ptr(&s), service))
    })
}

/// Puts what `service`'s own check-in found into its Desk DM, when Desk
/// is on for its hub. Whether it was delivered.
pub(crate) async fn heartbeat_found(service: &Arc<AgentService>, found: &str) -> bool {
    match for_service(service) {
        Some(crew) => crew.heartbeat_found(service, found).await,
        None => false,
    }
}

/// The crew of the hub `state` serves.
pub(crate) fn for_state(state: &Arc<GatewayState>) -> Option<Arc<CrewHub>> {
    CREWS
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .find(|c| Arc::ptr_eq(&c.state, state))
}

/// Tells `service`'s crew a config was saved, so a new overseer gets its
/// tools and summaries at once, and Desk knows its scope as it is now.
pub(crate) fn config_saved(service: &AgentService) {
    if let Some(crew) = for_service(service) {
        crew.read.lock().remove(service.agent_id.as_str());
        crew.overseers();
        crew.poke.notify_one();
    }
}

/// Why `body`'s `[oversight]` can't be saved for `service` over `before`,
/// when the desk is on for its hub: the group it oversees must be one of
/// the sidebar's, and only a person changes who oversees what: never the
/// agent itself (`agent_tool`).
pub(crate) fn oversight_refused(
    service: &AgentService,
    before: Option<&str>,
    body: &str,
    via: &str,
) -> Option<String> {
    let oversight = |b: Option<&str>| {
        b.and_then(|b| toml::from_str::<toml::Table>(b).ok())
            .and_then(|t| t.get("oversight").cloned())
    };
    if via == "agent_tool" && oversight(before) != oversight(Some(body)) {
        return Some("Who oversees what is the person's to set, in Desk.".to_string());
    }
    let crew = for_service(service)?;
    match crate::agent_config_write::overseen(body)? {
        Scope::Group(g) if !crew.desk.is_group(&g) => {
            Some(workforce_common::oversight::NOT_A_GROUP.to_string())
        }
        _ => None,
    }
}

/// After the person saved `body` for `service` over `before`: a group it
/// no longer names, that no agent names now, has its boss taken off, and
/// the main overseer sees it from now on.
pub(crate) async fn oversight_saved(service: &AgentService, before: Option<&str>, body: &str) {
    let Some(crew) = for_service(service) else {
        return;
    };
    let Some(was) = before.and_then(group_named_in) else {
        return;
    };
    if group_named_in(body).as_deref() == Some(was.as_str()) {
        return;
    }
    let still = crew.named_on_disk(Some(&service.workspace)).contains(&was);
    if !still {
        crew.desk.unbound_group(&was).await;
    }
}

/// Closes the group `body` makes `service` the boss of, before it is
/// written, when it wasn't closed: that group, to be opened again by
/// [`not_closed`] if the write doesn't happen.
pub(crate) fn closing(service: &AgentService, body: &str) -> Option<String> {
    let crew = for_service(service)?;
    match crate::agent_config_write::overseen(body)? {
        Scope::Group(g) if crew.desk.bound_groups_now([g.clone()]) => Some(g),
        _ => None,
    }
}

/// The write [`closing`] closed `group` for didn't happen: opened again,
/// past and all, unless a config names it.
pub(crate) async fn not_closed(service: &AgentService, group: &str) {
    if let Some(crew) = for_service(service) {
        if !crew.named_on_disk(None).iter().any(|g| g == group) {
            crew.desk.forget_bound(group).await;
        }
    }
}

/// Keeps a group [`closing`] closed, now its boss's config is written.
pub(crate) async fn keep_closed(service: &AgentService) {
    if let Some(crew) = for_service(service) {
        crew.desk.keep_boundaries().await;
    }
}

/// Whether the overseer `caller`, when it is one, may change or delete the
/// agent `target`: only an agent of its own scope that oversees nothing.
pub(crate) fn boss_may_change(caller: &str, target: &str) -> Result<(), String> {
    let crew = CREWS
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .find(|c| c.state.registry.get(caller).is_some());
    let Some(crew) = crew else {
        return Ok(());
    };
    let Some((_, o)) = crew.overseer_named(caller) else {
        return Ok(());
    };
    if caller == target {
        return Ok(());
    }
    let seen = crew.desk.seen_by(&o.scope);
    let other_boss = crew.overseer_named(target).is_some()
        || named_group(
            &crew
                .state
                .registry
                .get(target)
                .map(|s| s.workspace.clone())
                .unwrap_or_default(),
        )
        .is_some();
    if other_boss || !crew.agent_in(target, &seen) {
        return Err(crew.not_ours(&o.scope));
    }
    Ok(())
}

/// `service`'s settings as they are run: while any group has a boss of its
/// own, an overseer recalls only its own memories, so no other agent's
/// shared ones reach a client's boss or the main overseer.
pub(crate) fn recall_kept_apart(
    service: &AgentService,
    mut next: crate::hub_core::resolved_config::ResolvedConfig,
) -> crate::hub_core::resolved_config::ResolvedConfig {
    let Some(crew) = for_service(service) else {
        return next;
    };
    if crew.oversight_of(service).enabled && !crew.desk.bossed().is_empty() {
        next.memory.agent_recall_scope = workforce_common::settings::AgentRecallScope::Isolated;
    }
    next
}

/// What `service` is shown as in Desk: its own name, never its id; one
/// with none is "Boss" when it oversees the crew, else "Agent".
pub(crate) fn name_of(service: &AgentService) -> String {
    let id = service.agent_id.as_str();
    let name = service.display_name();
    let unnamed = name
        .as_deref()
        .is_none_or(|n| n.trim().is_empty() || n == id);
    let boss = unnamed
        && AgentWorkspaceConfig::load(&service.workspace)
            .ok()
            .flatten()
            .and_then(|c| c.oversight().ok())
            .is_some_and(|o| o.enabled);
    workforce_desk::crew::shown_name(id, name.as_deref(), boss)
}

/// The group `workspace`'s config.toml names in `[oversight] group`, read as
/// leniently as can be: off, broken or not, it says a group has a boss.
pub(crate) fn named_group(workspace: &Path) -> Option<String> {
    let path = workspace.join(workforce_common::config_files::WORKSPACE_CONFIG);
    let body = workforce_common::config_files::read(&path).ok().flatten()?;
    group_named_in(&body)
}

/// The group `body` names in `[oversight] group`, however the rest reads.
pub(crate) fn group_named_in(body: &str) -> Option<String> {
    let doc: toml::Table = toml::from_str(body).ok()?;
    let g = doc.get("oversight")?.get("group")?.as_str()?.trim();
    (!g.is_empty() && g != workforce_desk::UNGROUPED).then(|| g.to_string())
}

fn mtime(workspace: &Path) -> Option<SystemTime> {
    std::fs::metadata(workspace.join(workforce_common::config_files::WORKSPACE_CONFIG))
        .and_then(|m| m.modified())
        .ok()
}

impl CrewHub {
    /// Starts the crew for the hub `state` serves: the overseer's tools and
    /// summaries kept in step with its config, and the watcher that wakes it.
    pub fn start(
        state: Arc<GatewayState>,
        desk: Desk,
        timing: Timing,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        let crew = Arc::new_cyclic(|me| Self {
            me: me.clone(),
            state,
            desk,
            timing,
            running: Mutex::default(),
            hops: Mutex::default(),
            wake_hop: Mutex::default(),
            unbossed: Mutex::default(),
            manager_turns: Mutex::default(),
            listed: Mutex::default(),
            sent: Mutex::default(),
            read: Mutex::default(),
            tooled: Mutex::default(),
            poke: tokio::sync::Notify::new(),
            questions: Mutex::default(),
            work: Mutex::default(),
            starting: tokio::sync::Mutex::new(()),
            plan_failed: Mutex::default(),
            plan_failures: Mutex::default(),
        });
        {
            let mut crews = CREWS.lock();
            crews.retain(|c| c.strong_count() > 0);
            crews.push(Arc::downgrade(&crew));
        }
        // Desk knows the bosses and the boundaries before any line is
        // answered: never one scope for a moment at start.
        crew.overseers();
        crew.desk
            .answer_lines_with(&(Arc::clone(&crew) as Arc<dyn Crew>));
        tokio::spawn(watcher::sync_loop(Arc::clone(&crew), cancel.clone()));
        tokio::spawn(watcher::wake_loop(Arc::clone(&crew), cancel.clone()));
        tokio::spawn(plan::plan_loop(Arc::clone(&crew), cancel.clone()));
        tokio::spawn(line::line_loop(Arc::clone(&crew), cancel));
        crew
    }

    pub fn desk(&self) -> &Desk {
        &self.desk
    }

    fn me(&self) -> Arc<CrewHub> {
        self.me
            .upgrade()
            .expect("the crew is alive while it is used")
    }

    fn services(&self) -> Vec<Arc<AgentService>> {
        self.state.registry.all_services()
    }

    /// `service`'s `[oversight]` as its config.toml says now; off when it
    /// says nothing or something the hub can't use.
    pub(crate) fn oversight_of(&self, service: &AgentService) -> Oversight {
        let at = mtime(&service.workspace);
        let id = service.agent_id.as_str().to_string();
        if let Some((seen, o)) = self.read.lock().get(&id) {
            if *seen == at {
                return o.clone();
            }
        }
        let o = AgentWorkspaceConfig::load(&service.workspace)
            .ok()
            .flatten()
            .and_then(|c| c.oversight().ok())
            .unwrap_or_default();
        self.read.lock().insert(id, (at, o.clone()));
        o
    }

    /// The overseers and their settings, one per scope: the agents whose
    /// config says so (the most recently written in a scope, should two),
    /// a boss's group being one of the sidebar's. The main overseer first,
    /// then the groups' bosses by group. Desk is told, so its own rules keep
    /// to the same scopes.
    pub(crate) fn overseers(&self) -> Vec<(Arc<AgentService>, Oversight)> {
        let mut by_scope: HashMap<Scope, (Arc<AgentService>, Oversight)> = HashMap::new();
        // Every group any agent's config names for a boss is a boundary,
        // whether that config loads, is on, or its agent serves: it stays
        // one until the person takes the boss off it.
        // In force at once; kept for the next start behind it.
        if self.desk.bound_groups_now(self.named_on_disk(None)) {
            let desk = self.desk.clone();
            tokio::spawn(async move { desk.keep_boundaries().await });
        }
        for s in self.services() {
            let o = self.oversight_of(&s);
            if !o.enabled {
                continue;
            }
            if let Scope::Group(g) = &o.scope {
                if !self.desk.is_group(g) {
                    continue;
                }
            }
            match by_scope.get(&o.scope) {
                Some((kept, _)) if mtime(&kept.workspace) > mtime(&s.workspace) => {}
                _ => {
                    by_scope.insert(o.scope.clone(), (s, o));
                }
            }
        }
        let mut list: Vec<(Arc<AgentService>, Oversight)> = by_scope.into_values().collect();
        list.sort_by(|(_, a), (_, b)| a.scope.group().cmp(&b.scope.group()));
        self.desk.set_bosses(
            list.iter()
                .map(|(s, o)| workforce_desk::Boss {
                    agent: s.agent_id.as_str().to_string(),
                    name: name_of(s),
                    scope: o.scope.clone(),
                    level: o.level.as_str().to_string(),
                })
                .collect(),
        );
        list
    }

    /// Every group an agent's config.toml on disk names in `[oversight]
    /// group`: every agent's whose workspace is beside one of this hub's,
    /// served or not; `except` one workspace.
    pub(crate) fn named_on_disk(&self, except: Option<&Path>) -> Vec<String> {
        let mut dirs: Vec<std::path::PathBuf> = self
            .services()
            .iter()
            .filter_map(|s| s.workspace.parent().map(Path::to_path_buf))
            .collect();
        dirs.sort();
        dirs.dedup();
        let mut named = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let path = e.path();
                if except.is_some_and(|x| x == path) {
                    continue;
                }
                named.extend(named_group(&path));
            }
        }
        named
    }

    /// The overseer of `scope` and its settings, while it has one.
    pub(crate) fn overseer_for(&self, scope: &Scope) -> Option<(Arc<AgentService>, Oversight)> {
        self.overseers()
            .into_iter()
            .find(|(_, o)| o.scope == *scope)
    }

    /// The overseer `agent` and its settings, while it oversees.
    pub(crate) fn overseer_named(&self, agent: &str) -> Option<(Arc<AgentService>, Oversight)> {
        self.overseers()
            .into_iter()
            .find(|(s, _)| s.agent_id.as_str() == agent)
    }

    /// The overseer of the scope a row of `group` is in now.
    pub(crate) fn overseer_of_group(
        &self,
        group: Option<&str>,
    ) -> Option<(Arc<AgentService>, Oversight)> {
        // The one whose scope sees it: none for a group gone from the
        // sidebar.
        self.overseers()
            .into_iter()
            .find(|(_, o)| self.desk.seen_by(&o.scope).sees(group))
    }

    /// The overseer of the scope `session` is in now.
    pub(crate) fn overseer_of_session(
        &self,
        session: uuid::Uuid,
    ) -> Option<(Arc<AgentService>, Oversight)> {
        let group = self
            .desk
            .group_of_member(&workforce_desk::session_item(session));
        self.overseer_of_group(group.as_deref())
    }

    /// What `scope` is called where an overseer reads it: its group's name;
    /// none for the main overseer's.
    pub(crate) fn scope_name(&self, scope: &Scope) -> Option<String> {
        scope
            .group()
            .map(|g| self.desk.group_name(g).unwrap_or_else(|| g.to_string()))
    }

    /// The refusal of an id outside `scope`, in the words its overseer reads.
    pub(crate) fn not_ours(&self, scope: &Scope) -> String {
        workforce_desk::scope::not_ours(self.scope_name(scope).as_deref())
    }

    /// The main overseer: over everything ungrouped. For tests that set up
    /// one overseer; the hub always says which scope it means.
    #[cfg(test)]
    pub(crate) fn overseer_service(&self) -> Option<(Arc<AgentService>, Oversight)> {
        self.overseer_for(&Scope::Ungrouped)
    }

    /// The person whose Desk this is: who last spoke with `preferred` on
    /// Desk, else with any agent.
    pub(crate) fn person(&self, preferred: Option<&AgentService>) -> Option<String> {
        let latest = |s: &AgentService| {
            s.sessions
                .list(Some(DESK_CHANNEL), 1)
                .ok()?
                .into_iter()
                .next()
                .map(|s| (s.updated_at, s.owner))
        };
        preferred.and_then(latest).map(|(_, o)| o).or_else(|| {
            self.services()
                .iter()
                .filter_map(|s| latest(s))
                .max_by_key(|(at, _)| *at)
                .map(|(_, o)| o)
        })
    }

    /// The overseer `service` acting, `reason` its stated reason, at the
    /// hop of the wake-up it answers.
    pub(crate) fn by_boss(&self, service: &AgentService, reason: &str) -> By {
        let id = service.agent_id.as_str();
        let mut by = By::manager(id, &name_of(service), reason);
        by.hop = self.wake_hop.lock().get(id).map_or(1, |h| h + 1);
        by
    }

    /// The main overseer acting: for tests that set up one overseer.
    #[cfg(test)]
    pub(crate) fn by_overseer(&self, reason: &str) -> Option<By> {
        let (service, _) = self.overseer_service()?;
        Some(self.by_boss(&service, reason))
    }

    fn party_of_agent(&self, id: &str) -> Option<Party> {
        let service = self.state.registry.get(id)?;
        Some(Party::agent(id, &name_of(&service)))
    }

    /// The person spoke: the overseer may message the crew again.
    pub(crate) fn person_spoke(&self) {
        self.sent.lock().clear();
    }
}

#[async_trait]
impl Crew for CrewHub {
    /// Every agent whose config names `group` for a boss is switched off
    /// and names it no more, through the same write as any change (history,
    /// admission, hot reload), so the group opens to the main overseer from
    /// now on: before it is deleted.
    async fn release_group(&self, group: &str) -> Result<(), String> {
        // First agents the hub doesn't serve now whose config still names
        // it, so taking a served boss off below finds nothing else naming it.
        for u in crate::agent_config_write::unserved_beside(&self.services()) {
            crate::agent_config_write::unname_unserved(&u, self.state.def_store.as_deref(), group)
                .await
                .map_err(|e| {
                    if e.starts_with(&format!("{}'s", u.who())) {
                        e
                    } else {
                        format!("{} still names this group for its boss: {e}", u.who())
                    }
                })?;
        }
        for s in self.services() {
            if named_group(&s.workspace).as_deref() != Some(group) {
                continue;
            }
            let path = s
                .workspace
                .join(workforce_common::config_files::WORKSPACE_CONFIG);
            let Ok(Some(current)) = workforce_common::config_files::read(&path) else {
                continue;
            };
            let Some(off) = crate::agent_config_write::oversight_off(&current) else {
                return Err(format!(
                    "{}'s settings file has a mistake in it, so it still oversees this group.",
                    name_of(&s)
                ));
            };
            let id = s.agent_id.as_str().to_string();
            crate::agent_config_write::save(
                &s,
                &id,
                self.state.def_store.as_deref(),
                &off,
                workforce_orchestrator::workflow_store::EditContext::new("oversight", None),
                |now| now == Some(current.as_str()),
            )
            .await
            .map_err(|e| format!("{} still oversees this group: {e}", name_of(&s)))?;
        }
        // Named by no one now: open from now on, whoever was its boss.
        if !self.named_on_disk(None).iter().any(|g| g == group) {
            self.desk.unbound_group(group).await;
        }
        Ok(())
    }

    /// The main overseer: over everything ungrouped.
    fn overseer(&self) -> Option<Overseer> {
        let (service, o) = self.overseer_for(&Scope::Ungrouped)?;
        Some(Overseer {
            id: service.agent_id.as_str().to_string(),
            name: name_of(&service),
            level: o.level.as_str().to_string(),
            quiet_minutes: o.wake.quiet_minutes,
        })
    }

    fn agents(&self) -> Vec<CrewAgent> {
        let running = self.running.lock().clone();
        self.services()
            .iter()
            .map(|s| {
                let id = s.agent_id.as_str().to_string();
                let live = running
                    .values()
                    .filter(|r| r.agent_id == id)
                    .max_by_key(|r| r.since)
                    .map(|r| LiveTurn {
                        task: r.task.clone(),
                        detail: r.detail.clone(),
                        since: r.since,
                        last_activity: r.last_activity,
                    });
                CrewAgent {
                    name: name_of(s),
                    id,
                    live,
                }
            })
            .collect()
    }

    fn default_engine(&self, agent: &str) -> Option<String> {
        self.overseer_named(agent).map(|(_, o)| o.work.engine)
    }

    fn agent_name(&self, id: &str) -> Option<String> {
        self.state
            .registry
            .get(id)
            .and_then(|s| s.display_name())
            .filter(|n| !n.trim().is_empty() && n != id)
    }

    fn timezone(&self) -> chrono_tz::Tz {
        // The hub's: on a hub that serves no agents, its plumbing's config.
        self.state
            .registry
            .host()
            .map_or(chrono_tz::UTC, |s| s.config.user_timezone())
    }

    fn midnight(&self) -> DateTime<Utc> {
        let tz = self.timezone();
        let today = Utc::now().with_timezone(&tz).date_naive();
        today
            .and_hms_opt(0, 0, 0)
            .and_then(|m| m.and_local_timezone(tz).earliest())
            .map(|m| m.with_timezone(&Utc))
            .unwrap_or_else(|| Utc::now() - chrono::Duration::hours(24))
    }

    async fn accept(&self, suggestion: &Suggestion) -> Result<(), String> {
        let action = actions::Action::from_suggestion(&suggestion.action)?;
        let mut by = By::you();
        by.suggestion_id = Some(suggestion.id.clone());
        self.me().perform(&action, by).await.map(drop)
    }

    /// The needs of `person`, else of the person whose Desk this is, as the
    /// floor counts them.
    async fn open_needs(&self, person: Option<&str>) -> Option<Vec<Need>> {
        let floor = crate::floor::installed()?.clone();
        let desk = self.desk.clone();
        let sources = crate::desk_floor::Sources::new(floor, Arc::new(move || Some(desk.clone())));
        let caller = crate::desk_floor::FloorCaller {
            id: person
                .map(str::to_string)
                .or_else(|| self.person(None))
                .unwrap_or_default(),
            personal: true,
        };
        crate::desk_floor::open_needs(&self.state, &sources, &caller)
            .await
            .inspect_err(|e| {
                tracing::warn!(target: "floor", error = %format!("{e:#}"), "the floor's needs are unavailable")
            })
            .ok()
    }

    async fn asks(&self, reply: &str, group: Option<&str>) -> Option<bool> {
        self.read_reply(reply, group).await
    }

    fn answers_lines(&self) -> bool {
        !self.overseers().is_empty()
    }

    async fn assign(&self, agent: &str, task: &str) -> Result<String, String> {
        let action = actions::Action::Assign {
            agent: agent.to_string(),
            task: task.to_string(),
        };
        self.me().perform(&action, By::you()).await
    }

    async fn assign_as(&self, agent: &str, task: &str, by: &By) -> Result<String, String> {
        let action = actions::Action::Assign {
            agent: agent.to_string(),
            task: task.to_string(),
        };
        self.me().perform(&action, by.clone()).await
    }
}

/// Runs `service`'s morning or evening summary, for the scheduler.
pub(crate) async fn run_summary(
    service: &Arc<AgentService>,
    evening: bool,
) -> Result<String, String> {
    let crew = for_service(service).ok_or("Desk is not on in this hub")?;
    crew.summary(service, evening).await
}
