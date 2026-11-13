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
use workforce_common::oversight::{Level, Oversight, Scope};
use workforce_common::settings::AgentWorkspaceConfig;
use workforce_desk::crew::{By, Crew, CrewAgent, LiveTurn, Overseer, Party, Suggestion};
use workforce_desk::floor::Need;
use workforce_desk::Desk;

use crate::gateway::GatewayState;
use crate::serve::AgentService;

mod actions;
mod answer;
mod away;
mod board;
pub(crate) mod context;
pub(crate) mod decide;
mod decisions;
mod deliver;
mod extras;
mod handover;
mod items;
mod line;
mod observe;
mod own_settings;
mod plan;
mod preferences;
mod read_only;
mod reading;
pub(crate) mod relay;
pub(crate) mod remote_asks;
mod scope;
pub(crate) mod served;
mod spend;
mod tools;
mod updates;
pub(crate) use updates::HubWriter;
mod watcher;
mod work;

#[cfg(test)]
pub(crate) mod tests;

pub(crate) use tools::schema as crew_schema;
pub(crate) use watcher::reported;

pub(crate) use extras::mentions_for;
pub use handover::{handover_channel, HANDOVER};
pub use observe::Observed;
pub use plan::{plan_channel, PLAN};

/// What a check-in's prompt starts with: the person never typed it, so the
/// DM's history leaves it out.
pub const CHECK_IN: &str = crate::desk_surface::CHECK_IN;

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
    /// How many uplinks carry each remote boss's links now, by its
    /// `<hub>/<agent>`.
    remote_up: Mutex<HashMap<String, usize>>,
    /// When the overseer's turns in its DM started, by session.
    manager_turns: Mutex<HashMap<String, DateTime<Utc>>>,
    /// Pending approvals a wake-up listed, by the overseer's DM session.
    listed: Mutex<HashMap<String, Vec<workforce_desk::crew::MentionedApproval>>>,
    /// When the overseer messaged other agents since the person last spoke.
    sent: Mutex<Vec<DateTime<Utc>>>,
    /// Each agent's `[oversight]`, by the config.toml mtime it was read at.
    read: Mutex<HashMap<String, (Option<SystemTime>, Oversight)>>,
    /// The real money each overseer acting fully has spent today, in
    /// dollars, by agent, as last read ([`Self::read_spent_today`]): what
    /// keys paid for, never the person's plans. Act fully holds only under
    /// its budget, and not before this is read.
    spent_today: Mutex<HashMap<String, f64>>,
    /// The overseers the tools were last set up for, by agent: each one's
    /// scope and level.
    tooled: Mutex<HashMap<String, (Scope, String)>>,
    /// Ask the sync loop to look again now (a config was saved).
    poke: tokio::sync::Notify,
    /// The words of sessions' questions on Desk's line the overseer may
    /// answer, by message: an answer is gated by what was asked as well as
    /// by what it says. One not known here is the person's.
    questions: Mutex<HashMap<uuid::Uuid, String>>,
    /// How many decisions (and suggestions) each boss made, by the session
    /// they were about: a line or work turn that moved none let it wait.
    acted: Mutex<HashMap<(String, Option<uuid::Uuid>), u64>>,
    /// When each boss last asked the person with `crew.ask_person`: its
    /// turn's reply then isn't also kept as a question of its own.
    asked_person: Mutex<HashMap<String, DateTime<Utc>>>,
    /// The sessions boss turns acted on lately (started, told, handed
    /// work), by the turn's session id, and when: what a DM turn did for
    /// the person's order. Another turn's acts are never its own.
    turn_acts: Mutex<Vec<(String, DateTime<Utc>, uuid::Uuid)>>,
    /// The person's messages each boss DM turn is answering, by session.
    person_turns: Mutex<HashMap<String, String>>,
    /// The DM sessions running a message another overseer sent on: not
    /// the person's, so never their order.
    relayed_turns: Mutex<std::collections::HashSet<String>>,
    /// Who writes the updates' paragraphs; none: plainly.
    update_writer: Mutex<Option<Arc<dyn updates::UpdateWriter>>>,
    /// The bosses whose update is being looked at now.
    updating: Mutex<std::collections::HashSet<String>>,
    /// The suggestions each boss's quiet check-ins made, and when, for its
    /// next update to show.
    quiet_made:
        Mutex<HashMap<String, Vec<(DateTime<Utc>, workforce_desk::crew::MentionedSuggestion)>>>,
    /// The person typing in each boss's DM, by its agent: what it would
    /// post waits for a pause.
    dm_typing: Mutex<HashMap<String, workforce_desk::away::Typing>>,
    /// What Desk last said of the person: its window attended, and which
    /// DM is on screen.
    presence: Mutex<Option<workforce_desk::away::Presence>>,
    /// When the person last had each boss's DM on screen, as Desk said.
    on_screen: Mutex<HashMap<String, DateTime<Utc>>>,
    /// Wakes the updates' look at once: the person paused typing, came
    /// back, or wrote.
    update_poke: tokio::sync::Notify,
    /// Wakes what waits on the person's typing: they sent.
    typing_done: tokio::sync::Notify,
    /// How far the person read each boss's DM, for a test whose sessions
    /// keep no read points.
    #[cfg(test)]
    test_reads: Mutex<HashMap<String, DateTime<Utc>>>,
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
    /// Sessions a start's work was given to, held from other starts until
    /// [`actions::REUSE_CLAIM`] has passed.
    reuse_claims: Mutex<HashMap<uuid::Uuid, std::time::Instant>>,
    /// Calls for the person's machines, waiting for Desk's native side
    /// ([`relay`]), and who waits for each answer.
    relays: Mutex<relay::Relays>,
    /// Told when a call is queued for a machine.
    relay_queued: tokio::sync::Notify,
    /// How long a relayed call waits for its machine's answer.
    relay_wait: Mutex<Duration>,
    /// When Desk's native side last asked for calls: while it hasn't
    /// lately, nothing is queued and a machine is said not reached at once.
    relay_polled: Mutex<Option<std::time::Instant>>,
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

/// What a turn of `service`'s in its Desk DM is told of how much it may do
/// on its own, when it oversees the crew: its level now and the switches
/// that are off. `None` for an agent that doesn't oversee.
pub(crate) fn dm_oversight_hint(service: &AgentService) -> Option<String> {
    let crew = for_service(service)?;
    let (service, o) = crew.overseer_named(service.agent_id.as_str())?;
    let (level, _) = crew.level_now(service.agent_id.as_str(), &o);
    let unbound = o.level != Level::Watch && crew.unbound_given(service.agent_id.as_str(), &o);
    Some(CrewHub::dm_hint(level, &o, unbound))
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

/// Why a Desk room turn for `service` mustn't run: the room is one whose
/// group's boundary keeps it out now (a boss moved away from it, or an
/// agent that left), whatever the Desk app sent. `None` off a room, or
/// without a crew.
pub(crate) async fn room_turn_refused(
    service: &AgentService,
    place: &workforce_common::channels::desk::DeskPlace,
) -> Option<String> {
    use workforce_common::channels::desk::DeskPlace;
    let crew = for_service(service)?;
    let room = match place {
        DeskPlace::Room { room, .. } => *room,
        // An older Desk names a room by its slug (or id): the same room.
        DeskPlace::LegacyRoom { slug } => crew.desk.room_named(slug).await?,
        _ => return None,
    };
    match crew
        .desk
        .agent_reaches_room(room, service.agent_id.as_str())
        .await
    {
        true => None,
        false => {
            Some("This agent isn't in that room's group now, so it can't answer there.".into())
        }
    }
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

/// Whether Act fully is in force for `o`, having spent `spent` dollars of
/// real money today in its scope (`None`: not read), or why not in plain
/// words.
pub(crate) fn fully_given(
    o: &Oversight,
    spent: Option<f64>,
) -> Result<&workforce_common::oversight::Full, String> {
    let full = o.fully().map_err(|why| {
        why.unwrap_or("It acts within Desk: Act fully isn't chosen.")
            .to_string()
    })?;
    // Anywhere with no budget set, nothing it spends holds it.
    let Some(budget) = full.budget_usd_per_day() else {
        return Ok(full);
    };
    match spent {
        None => Err(FULL_SPEND_UNREAD.into()),
        Some(spent) if spent >= budget => Err(full_spent_why(spent, budget)),
        Some(_) => Ok(full),
    }
}

/// Why Act fully isn't in force for a moment, at a start: today's spend
/// isn't read yet.
pub(crate) const FULL_SPEND_UNREAD: &str =
    "Act fully waits until today's spend is read, so it acts within Desk for now.";

/// [`fully_given`] for the overseer `agent`: a boss served from a grant
/// acts fully only under a daily budget its grant gives; this hub's own
/// may leave it blank.
pub(crate) fn fully_for<'o>(
    agent: &str,
    o: &'o Oversight,
    spent: Option<f64>,
) -> Result<&'o workforce_common::oversight::Full, String> {
    if workforce_desk::grant::is_served(agent)
        && o.level == Level::Full
        && o.full.budget_cents_per_day.is_none()
    {
        return Err(workforce_common::oversight::FULL_NO_BUDGET.to_string());
    }
    fully_given(o, spent)
}

/// Why Act fully stopped, its budget spent: what it counts, what stopped,
/// and what the person can do.
pub(crate) fn full_spent_why(spent: f64, budget: f64) -> String {
    format!(
        "Act fully's daily budget is spent: ${spent:.2} of ${budget:.2} in real money today (calls paid by an API key, Bedrock or Vertex, in this scope; Claude and ChatGPT plans aren't counted). So it stopped merging, tagging, releasing and running workflows on its own, and acts within Desk until tomorrow, or until the person raises or clears the budget in its Desk settings."
    )
}

/// Where today's count of Act fully's budget starts: midnight in the
/// person's time, or when they last cleared it, if that is later; never
/// after `now` (a time ahead of the clock would count nothing until then).
pub(crate) fn counted_from(
    midnight: DateTime<Utc>,
    now: DateTime<Utc>,
    full: &workforce_common::oversight::Full,
) -> DateTime<Utc> {
    cleared_today(midnight, now, full).unwrap_or(midnight)
}

/// When the person cleared today's count, as it counts: today's, and
/// never after `now`.
fn cleared_today(
    midnight: DateTime<Utc>,
    now: DateTime<Utc>,
    full: &workforce_common::oversight::Full,
) -> Option<DateTime<Utc>> {
    full.budget_cleared_at
        .map(|c| c.min(now))
        .filter(|c| *c > midnight)
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
            remote_up: Mutex::default(),
            manager_turns: Mutex::default(),
            listed: Mutex::default(),
            sent: Mutex::default(),
            read: Mutex::default(),
            spent_today: Mutex::default(),
            tooled: Mutex::default(),
            poke: tokio::sync::Notify::new(),
            questions: Mutex::default(),
            acted: Mutex::default(),
            asked_person: Mutex::default(),
            turn_acts: Mutex::default(),
            person_turns: Mutex::default(),
            relayed_turns: Mutex::default(),
            update_writer: Mutex::default(),
            updating: Mutex::default(),
            quiet_made: Mutex::default(),
            dm_typing: Mutex::default(),
            presence: Mutex::default(),
            on_screen: Mutex::default(),
            update_poke: tokio::sync::Notify::new(),
            typing_done: tokio::sync::Notify::new(),
            #[cfg(test)]
            test_reads: Mutex::default(),
            work: Mutex::default(),
            starting: tokio::sync::Mutex::new(()),
            plan_failed: Mutex::default(),
            plan_failures: Mutex::default(),
            reuse_claims: Mutex::default(),
            relays: Mutex::default(),
            relay_queued: tokio::sync::Notify::new(),
            relay_wait: Mutex::new(Duration::from_secs(30)),
            relay_polled: Mutex::new(None),
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
        tokio::spawn(decide::decide_loop(Arc::clone(&crew), cancel.clone()));
        tokio::spawn(answer::answer_loop(Arc::clone(&crew), cancel.clone()));
        tokio::spawn(plan::plan_loop(Arc::clone(&crew), cancel.clone()));
        tokio::spawn(items::items_loop(Arc::clone(&crew), cancel.clone()));
        tokio::spawn(updates::updates_loop(Arc::clone(&crew), cancel.clone()));
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
            // A `/` is how a boss served from a grant is known
            // (`<hub>/<agent>`): no agent of this hub is ever taken for one,
            // nor oversees under such an id.
            if workforce_desk::grant::is_served(s.agent_id.as_str()) {
                continue;
            }
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
        // The level each runs at: Act fully only while it is in force.
        let level = |s: &AgentService, o: &Oversight| match o.level {
            Level::Full if self.fully(s.agent_id.as_str(), o).is_err() => Level::Act,
            l => l,
        };
        self.desk.set_bosses(
            list.iter()
                .map(|(s, o)| workforce_desk::Boss {
                    agent: s.agent_id.as_str().to_string(),
                    name: name_of(s),
                    scope: o.scope.clone(),
                    level: level(s, o).as_str().to_string(),
                })
                .collect(),
        );
        // The cheap model's day for every session's account: the main
        // boss's, the accounts being shared (else the first boss's).
        self.desk.set_account_limits(
            list.iter()
                .find(|(_, o)| o.scope == Scope::Ungrouped)
                .or(list.first())
                .map(|(_, o)| workforce_desk::account::Limits {
                    cheap_calls_per_day: o.updates.cheap_calls_per_day,
                    account_questions_per_day: o.updates.account_questions_per_day,
                })
                .unwrap_or_default(),
        );
        // No boss allows a change to any agent's own settings, its own
        // limits among them.
        self.desk.withhold_agent_settings(
            self.services()
                .iter()
                .flat_map(|s| crate::desk_computer::own_config(&s.workspace))
                .collect(),
        );
        self.desk.set_full(
            list.iter()
                .filter_map(|(s, o)| {
                    let full = self.fully(s.agent_id.as_str(), o).ok()?;
                    Some((
                        s.agent_id.as_str().to_string(),
                        workforce_desk::charge::FullGrant {
                            repos: full.repos.clone(),
                            workflows: full.workflows.clone(),
                            anywhere: full.anywhere,
                            unbound: self.unbound_given(s.agent_id.as_str(), o),
                        },
                    ))
                })
                .collect(),
        );
        // Who won't answer what the sessions it runs ask, and why: the
        // badge of such a session says it waits on the person.
        self.desk.set_unanswering(
            list.iter()
                .filter_map(|(s, o)| {
                    let name = name_of(s);
                    let why = match o.level {
                        Level::Watch => format!("{name} only watches."),
                        Level::Suggest => format!("{name} only suggests: you decide."),
                        Level::Act | Level::Full => {
                            let held = self.desk.agent_held_why(s.agent_id.as_str())?;
                            format!("{name} is {held}.")
                        }
                    };
                    Some((s.agent_id.as_str().to_string(), why))
                })
                .chain(
                    self.remote_unanswering(
                        &list
                            .iter()
                            .map(|(_, o)| o.scope.clone())
                            .collect::<Vec<_>>(),
                    ),
                )
                .collect(),
        );
        self.desk.set_holds(
            list.iter()
                .map(|(s, o)| {
                    (
                        s.agent_id.as_str().to_string(),
                        chrono::Duration::minutes(o.wake.hold_minutes.into()),
                    )
                })
                .collect(),
        );
        list
    }

    /// Whether the overseer `agent`, with settings `o`, acts fully now, or
    /// why not in plain words: it chose Act fully, named repositories and a
    /// daily budget, and today's spend in its scope has been read and is
    /// under that budget. Short of any of it, it acts within Desk.
    pub(crate) fn fully<'o>(
        &self,
        agent: &str,
        o: &'o Oversight,
    ) -> Result<&'o workforce_common::oversight::Full, String> {
        let spent = self.spent_today.lock().get(agent).copied();
        fully_for(agent, o, spent)
    }

    /// The level the overseer `agent` runs at now, and, when it chose Act
    /// fully but that isn't in force, why it acts within Desk instead.
    pub(crate) fn level_now(&self, agent: &str, o: &Oversight) -> (Level, Option<String>) {
        match (o.level, self.fully(agent, o)) {
            (Level::Full, Ok(_)) => (Level::Full, None),
            (Level::Full, Err(why)) => (Level::Act, Some(why)),
            (l, _) => (l, None),
        }
    }

    /// `by`, the overseer `agent` acting on its own, as Activity records
    /// it: what it does while Act fully is in force says so.
    pub(crate) fn by_at_level(&self, mut by: By, o: &Oversight) -> By {
        let unbound = self
            .overseer_named(&by.party.id)
            .is_some_and(|(s, _)| self.unbound_given(s.agent_id.as_str(), o));
        if unbound {
            by.reason = Some(match by.reason.take() {
                Some(r) => format!("{} (unbound: no limits)", r.trim_end()),
                None => "Unbound: no limits.".into(),
            });
            return by;
        }
        if self.level_now(&by.party.id, o).0 == Level::Full {
            by.reason = Some(match by.reason.take() {
                Some(r) => format!("{} (at Act fully)", r.trim_end()),
                None => "At Act fully.".into(),
            });
        }
        by
    }

    /// Whether the boss `agent`, overseeing with `o`, is unbound now: it
    /// is at Act fully ([`Oversight::unbound`]) and Act fully is in force
    /// (its budget, when set, isn't spent; a boss served from a grant needs
    /// one, [`fully_for`]). One check for every boss, this hub's own and
    /// one served from a grant.
    pub(crate) fn unbound_given(&self, agent: &str, o: &Oversight) -> bool {
        o.unbound() && self.fully(agent, o).is_ok()
    }

    /// Whether the overseer of `o`'s scope is unbound now.
    pub(crate) fn unbound(&self, o: &Oversight) -> bool {
        self.overseer_for(&o.scope)
            .is_some_and(|(s, _)| self.unbound_given(s.agent_id.as_str(), o))
    }

    /// Whether the overseer of `o`'s scope acts fully in the repository
    /// session `session` works in, now.
    pub(crate) fn ships_in(&self, session: uuid::Uuid, o: &Oversight) -> bool {
        let boss = self
            .overseer_for(&o.scope)
            .map(|(s, _)| s.agent_id.as_str().to_string());
        self.ships_in_for(session, o, boss.as_deref())
    }

    /// Whether `boss`, at `o`, acts fully in the repository session
    /// `session` works in, now.
    pub(crate) fn ships_in_for(
        &self,
        session: uuid::Uuid,
        o: &Oversight,
        boss: Option<&str>,
    ) -> bool {
        o.level == Level::Full
            && boss
                .is_some_and(|id| self.fully(id, o).is_ok() && self.desk.acts_fully_in(session, id))
    }

    /// Reads the real money each overseer that chose Act fully has spent
    /// today, for [`Self::fully`], and tells Desk, which shows it: its
    /// scope's Desk sessions' calls a key paid for, and the model calls of
    /// the agents in its scope (itself among them, once) the hub billed as
    /// metered. A call on the person's Claude or ChatGPT plan bills nobody,
    /// so it never counts, however much it would cost at API prices. One
    /// that can't be read isn't in force until it can be.
    pub(crate) async fn read_spent_today(&self, overseers: &[(Arc<AgentService>, Oversight)]) {
        let now = Utc::now();
        let midnight = self
            .desk
            .period_start(workforce_desk::ledger::Period::Day, now);
        let mut spent = HashMap::new();
        let mut shown = HashMap::new();
        for (s, o) in overseers {
            let Ok(full) = o.fully() else {
                continue;
            };
            let from = counted_from(midnight, now, full);
            let seen = self.desk.seen_by(&o.scope);
            let filter = workforce_desk::ledger_store::Filter {
                seen: Some(seen.clone()),
                ..Default::default()
            };
            let read = match self.desk.billed_usd(from, now, &filter).await {
                Ok(sessions) => s
                    .telemetry_store
                    .metered_spend(from, now)
                    .map(|rows| sessions + self.agents_billed(s, &seen, &rows)),
                Err(e) => Err(eyre::eyre!("{e}")),
            };
            let usd = match read {
                Ok(usd) => {
                    spent.insert(s.agent_id.as_str().to_string(), usd);
                    Some(usd)
                }
                Err(e) => {
                    tracing::warn!(
                        agent = s.agent_id.as_str(),
                        error = %e,
                        "couldn't read today's spend; Act fully waits until it can"
                    );
                    None
                }
            };
            if let Some(budget) = full.budget_usd_per_day() {
                shown.insert(
                    s.agent_id.as_str().to_string(),
                    workforce_desk::floor::FullBudget {
                        spent_usd: usd,
                        budget_usd: budget,
                        spent: usd.is_some_and(|u| u >= budget),
                        cleared_at: cleared_today(midnight, now, full),
                    },
                );
            }
        }
        *self.spent_today.lock() = spent;
        self.desk.set_full_budgets(shown);
    }

    /// What the hub billed as metered to the agents `overseer`'s scope
    /// sees, `overseer` itself among them, each once, of `rows`.
    fn agents_billed(
        &self,
        overseer: &AgentService,
        seen: &workforce_desk::scope::Seen,
        rows: &[workforce_common::persistence::MeteredSpend],
    ) -> f64 {
        let me = overseer.agent_id.as_str();
        rows.iter()
            .filter(|r| {
                r.agent_id == me || seen.sees(self.desk.group_of_agent(&r.agent_id).as_deref())
            })
            .map(|r| r.cost_usd)
            .sum()
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
        // The hub's memory, through any agent's handle: tagging names its
        // owner itself.
        let memory = self.services().iter().find_map(|s| s.memory_store.clone());
        for u in crate::agent_config_write::unserved_beside(&self.services()) {
            crate::agent_config_write::unname_unserved(
                &u,
                self.state.def_store.as_deref(),
                memory.as_deref(),
                group,
            )
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

    async fn ask_handover(&self, h: &workforce_desk::moves::Handover, facts: &str) {
        self.ask_handover_turn(h, facts).await;
    }

    async fn deliver_handover(&self, h: &workforce_desk::moves::Handover) -> Result<(), String> {
        self.deliver_handover_turn(h).await
    }

    async fn move_boss(&self, agent: &str, to: Option<&str>, replace: bool) -> Result<(), String> {
        self.move_boss_to(agent, to, replace).await
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

    async fn undo_decision(
        &self,
        id: uuid::Uuid,
    ) -> Result<String, workforce_desk::crew::UndoRefused> {
        self.me().undo(id).await
    }

    async fn answer_item(
        &self,
        id: uuid::Uuid,
        answer: &str,
    ) -> Result<String, workforce_desk::crew::ItemRefused> {
        self.me().answer_item(id, answer).await
    }

    async fn drop_item(&self, id: uuid::Uuid) -> Result<String, workforce_desk::crew::ItemRefused> {
        self.me().drop_item(id).await
    }

    async fn always_item(
        &self,
        id: uuid::Uuid,
        then: Option<&str>,
    ) -> Result<String, workforce_desk::crew::ItemRefused> {
        self.me().always_item(id, then).await
    }

    async fn keep_preference(
        &self,
        when: &str,
        then: &str,
        scope: &str,
        from_item: Option<uuid::Uuid>,
        boss_scope: Option<&str>,
    ) -> Result<workforce_desk::preference::Preference, workforce_desk::crew::ItemRefused> {
        self.person_prefers(when, then, scope, from_item, boss_scope)
            .await
    }

    async fn handled(
        &self,
        agent: Option<&str>,
        since: Option<DateTime<Utc>>,
    ) -> Option<serde_json::Value> {
        self.handled_counts(agent, since).await
    }

    fn dm_typing(&self, agent: &str) -> bool {
        self.typing_in(agent)
    }

    fn presence(&self, attending: bool, dm_on_screen: Option<&str>) {
        self.present(attending, dm_on_screen);
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
