//! The crew manager's hub side: who oversees the crew (`[oversight]`),
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
use workforce_common::oversight::Oversight;
use workforce_common::settings::AgentWorkspaceConfig;
use workforce_desk::crew::{By, Crew, CrewAgent, LiveTurn, Overseer, Party, Suggestion};
use workforce_desk::floor::Need;
use workforce_desk::Desk;

use crate::gateway::GatewayState;
use crate::serve::AgentService;

mod actions;
mod deliver;
mod extras;
mod observe;
mod reading;
mod spend;
mod tools;
mod watcher;

#[cfg(test)]
pub(crate) mod tests;

pub(crate) use extras::mentions_for;
pub use observe::Observed;

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
    /// The hop of the wake-up the overseer is answering, if it is.
    wake_hop: Mutex<Option<i16>>,
    /// When the overseer's turns in its DM started, by session.
    manager_turns: Mutex<HashMap<String, DateTime<Utc>>>,
    /// Pending approvals a wake-up listed, by the overseer's DM session.
    listed: Mutex<HashMap<String, Vec<workforce_desk::crew::MentionedApproval>>>,
    /// When the overseer messaged other agents since the person last spoke.
    sent: Mutex<Vec<DateTime<Utc>>>,
    /// Each agent's `[oversight]`, by the config.toml mtime it was read at.
    read: Mutex<HashMap<String, (Option<SystemTime>, Oversight)>>,
    /// The overseer the tools were last set up for, and at which level.
    tooled: Mutex<Option<(String, String)>>,
    /// Ask the sync loop to look again now (a config was saved).
    poke: tokio::sync::Notify,
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
/// tools and summaries at once.
pub(crate) fn config_saved(service: &AgentService) {
    if let Some(crew) = for_service(service) {
        crew.read.lock().remove(service.agent_id.as_str());
        crew.poke.notify_one();
    }
}

pub(crate) fn name_of(service: &AgentService) -> String {
    service
        .display_name()
        .unwrap_or_else(|| service.agent_id.as_str().to_string())
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
            manager_turns: Mutex::default(),
            listed: Mutex::default(),
            sent: Mutex::default(),
            read: Mutex::default(),
            tooled: Mutex::default(),
            poke: tokio::sync::Notify::new(),
        });
        {
            let mut crews = CREWS.lock();
            crews.retain(|c| c.strong_count() > 0);
            crews.push(Arc::downgrade(&crew));
        }
        tokio::spawn(watcher::sync_loop(Arc::clone(&crew), cancel.clone()));
        tokio::spawn(watcher::wake_loop(Arc::clone(&crew), cancel));
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

    /// The agent overseeing the crew and its settings: the one whose config
    /// says so (the most recently written, should two).
    pub(crate) fn overseer_service(&self) -> Option<(Arc<AgentService>, Oversight)> {
        self.services()
            .into_iter()
            .map(|s| {
                let o = self.oversight_of(&s);
                (s, o)
            })
            .filter(|(_, o)| o.enabled)
            .max_by_key(|(s, _)| mtime(&s.workspace))
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

    /// The overseer acting, `reason` its stated reason, at the hop of the
    /// wake-up it answers.
    pub(crate) fn by_overseer(&self, reason: &str) -> Option<By> {
        let (service, _) = self.overseer_service()?;
        let mut by = By::manager(service.agent_id.as_str(), &name_of(&service), reason);
        by.hop = self.wake_hop.lock().map_or(1, |h| h + 1);
        Some(by)
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
    fn overseer(&self) -> Option<Overseer> {
        let (service, o) = self.overseer_service()?;
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

    fn timezone(&self) -> chrono_tz::Tz {
        self.services()
            .first()
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

    async fn asks(&self, reply: &str) -> Option<bool> {
        self.read_reply(reply).await
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
