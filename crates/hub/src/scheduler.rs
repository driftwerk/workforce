use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use uuid::Uuid;

use workforce_common::channels::OutboundMessage;
use workforce_common::subscription::{is_automated_source, Payer};

pub(crate) use crate::hub_core::origins::{automated_origin, descending_from};
pub(crate) use crate::hub_core::schedules::continues_conversation;
pub(crate) use crate::hub_core::schedules::{cron_next_run, next_run_time};
pub use crate::hub_core::schedules::{
    runs_per_day, DeliveryTarget, EditRefused, JobEdit, JobPayload, JobRunRecord, Outcomes,
    RecurringCost, Schedule, ScheduledJob, SessionMode, DELIVERY_SURFACE_VOICE, OWNER_TOKEN_EDITOR,
    VISIBILITY_GLOBAL, VISIBILITY_PRIVATE,
};
use crate::serve::AgentService;

/// Callback for delivering job results (e.g., to WebSocket clients, Slack, etc.).
pub type DeliveryCallback = Arc<
    dyn Fn(String, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// [`ScheduledJob::edited_by`] for a save through the scheduler tool in a
/// turn that names nobody.
pub const UNIDENTIFIED_EDITOR: &str = "tool:unidentified";

/// Normalise a caller identity for the owner-scope predicates below: an empty
/// string is treated as "no identity" so it can never match an empty/absent
/// owner. Mirrors `current_identity_id()`'s `filter(|u| !u.is_empty())`.
fn normalise_caller(caller_id: Option<&str>) -> Option<&str> {
    caller_id.filter(|c| !c.is_empty())
}

/// Whether `caller_id` may *see* `job` in a listing. Pure mirror of the
/// gateway's `schedule_visible` (`gateway.rs`): admin, owner, or a non-reminder
/// global job. Fail-closed: an absent/empty `caller_id` and non-admin sees only
/// non-reminder global jobs.
pub(crate) fn schedule_visible_to(
    job: &ScheduledJob,
    caller_id: Option<&str>,
    is_admin: bool,
) -> bool {
    let caller_id = normalise_caller(caller_id);
    is_admin
        || job.owner().is_some_and(|o| caller_id == Some(o))
        || (!job.is_reminder && job.effective_visibility() == VISIBILITY_GLOBAL)
}

/// Whether `caller_id` may *mutate* `job` (remove / run / read history). Pure
/// mirror of the gateway's `authorize_schedule_mutation`: admin or owner only —
/// a global job is visible to everyone but writable only by its owner.
/// Fail-closed: an absent/empty `caller_id` and non-admin can never mutate.
pub(crate) fn schedule_mutable_by(
    job: &ScheduledJob,
    caller_id: Option<&str>,
    is_admin: bool,
) -> bool {
    let caller_id = normalise_caller(caller_id);
    is_admin || job.owner().is_some_and(|o| caller_id == Some(o))
}

/// The note on a run of a pinned job the agent's `[scheduler]` config ran
/// fresh.
pub const RAN_FRESH_BY_POLICY: &str =
    "ran in a fresh session: this agent's config does not let recurring jobs pin one";

/// What the run record of a slot skipped at the workflow's concurrency cap
/// opens with.
pub const SKIPPED_PREFIX: &str = "skipped:";

/// The last `n` of `runs`, oldest first, classified.
pub fn outcomes<'a>(runs: impl IntoIterator<Item = &'a JobRunRecord>, n: usize) -> Outcomes {
    let runs: Vec<&JobRunRecord> = runs.into_iter().collect();
    runs[runs.len().saturating_sub(n)..]
        .iter()
        .fold(Outcomes::default(), |tally, run| {
            let considered = tally.considered + 1;
            match (run.success, run.output.starts_with(SKIPPED_PREFIX)) {
                (true, true) => Outcomes {
                    considered,
                    skipped: tally.skipped + 1,
                    ..tally
                },
                (true, false) => Outcomes {
                    considered,
                    started: tally.started + 1,
                    ..tally
                },
                (false, _) => Outcomes {
                    considered,
                    failed: tally.failed + 1,
                    ..tally
                },
            }
        })
}

/// Messaging context captured during an agent run: (channel_id, thread_id).
/// Per-session messaging context, keyed by session_key.
type MessagingContexts = HashMap<String, (String, Option<String>)>;

/// Consecutive delivery/run failures after which a recurring job disables.
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// Who a job created now belongs to: the person the turn runs as; the agent
/// itself (`agent:{agent_id}`) for a turn from an automated source
/// (`heartbeat`, `scheduler`, …), so the job runs as the agent rather than
/// as a sentinel; any other background principal (`system`, `system:*`,
/// `agent:*`), an unidentified or an outside caller as it is; nobody when no
/// identity is in scope. An outside caller (a game player) is never promoted
/// to the agent.
fn creator_of(payer: Payer, agent_id: &str) -> Option<String> {
    match payer {
        Payer::Person(person) => Some(person.to_string()),
        Payer::Background(id) if is_automated_source(&id) => Some(format!("agent:{agent_id}")),
        Payer::Background(id) | Payer::Unidentified(id) | Payer::External(id) => Some(id),
        Payer::Unset => None,
    }
}

/// Whether a job belongs to a person (or an agent acting for one) rather
/// than the system. System sentinels (`system:consolidation`, …) register
/// code-owned cadences and are exempt from the interval floor and job caps.
fn is_owned(creator: Option<&str>) -> bool {
    creator.is_some_and(|c| !c.starts_with("system:"))
}

/// Whether a job is its run-as person's own: they created it to run as
/// themselves and nobody else has changed it since
/// ([`ScheduledJob::edited_by`]). Who may read its history is theirs to
/// choose, as where they post from their own conversation is.
pub(crate) fn is_own(job: &ScheduledJob) -> bool {
    job.creator_identity_id
        .as_deref()
        .filter(|creator| !creator.is_empty())
        .is_some_and(|creator| {
            job.effective_run_as_identity() == Some(creator)
                && job
                    .edited_by
                    .as_deref()
                    .is_none_or(|editor| editor == creator)
        })
}

/// Who is in charge of a run of `job`, among `jobs`: the person it runs as
/// when the job is their own ([`is_own`]) and so is every job continuing the
/// same session, whose runs it replays. What the job posts, and where, is
/// that person's decision, as in their own conversation.
fn run_author(job: &ScheduledJob, jobs: &[ScheduledJob]) -> crate::serve::RunAuthor {
    let alone = job.pinned_session().is_none_or(|session| {
        jobs.iter()
            .filter(|other| other.pinned_session().as_ref() == Some(&session))
            .all(is_own)
    });
    if is_own(job) && alone {
        crate::serve::RunAuthor::Principal
    } else {
        crate::serve::RunAuthor::Other
    }
}

/// The session channel of one automated run of `id` from `source`, fresh per
/// fire so no run inherits another's transcript. Millisecond resolution keeps
/// a manual run apart from the scheduled tick it races. A scheduler run's
/// channel is also its cost records' `task_id`, which the job history joins on.
pub(crate) fn isolated_run_channel(source: &str, id: &str, at: DateTime<Utc>) -> String {
    format!("{source}:{id}:{}", at.timestamp_millis())
}

/// How often the leader releases the jobs a suspension holds whose person
/// was reactivated by a path that did not release them itself.
const HOLD_REVIEW_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// The fields of a stored job a run writes. `enabled`, the last, is among
/// them only when the run left the job switched off: a job its owner paused
/// on another instance while it ran stays paused.
const RUN_BOOKKEEPING: [&str; 5] = [
    "last_run",
    "next_run",
    "run_count",
    "delivery_failures",
    "enabled",
];

/// What every scheduler channel starts with, and the key minted for a pinned
/// job.
const JOB_SESSION_PREFIX: &str = "scheduler:";

/// Whether `channel` is a scheduler channel: the session a job keeps for
/// itself or for the person it runs as, or the one a single run was given.
pub(crate) fn is_job_session(channel: &str) -> bool {
    channel.starts_with(JOB_SESSION_PREFIX)
}

/// The scheduler runtime.
pub struct Scheduler {
    jobs: Arc<RwLock<Vec<ScheduledJob>>>,
    /// The bootstrap service this scheduler was built against. Jobs run
    /// against [`Self::service`], which prefers the owner once adopted.
    seed: Arc<AgentService>,
    owner: OnceLock<Weak<AgentService>>,
    /// Optional callback to deliver job results to connected clients.
    delivery: Option<DeliveryCallback>,
    /// Default timezone for cron evaluation when a job doesn't specify one.
    default_tz: Tz,
    /// Directory for persisting jobs and run history.
    data_dir: PathBuf,
    /// Run history ring buffer.
    run_history: Arc<RwLock<Vec<JobRunRecord>>>,
    /// Max history entries to keep.
    max_history: usize,
    /// Per-session messaging context (channel_id, thread_id) keyed by session_key.
    /// Used as fallback when the LLM doesn't pass these to the scheduler tool.
    messaging_contexts: Arc<RwLock<MessagingContexts>>,
    /// When set, only execute jobs if this flag is true (leader election).
    /// Uses `OnceLock` for interior mutability — set after construction via
    /// `set_leader_flag()` without requiring `&mut self`.
    leader: OnceLock<Arc<AtomicBool>>,
    /// Voice announcement delivery, for jobs whose `deliver_to` names a
    /// voice bridge. Same late-set `OnceLock` shape as the leader flag.
    voice_announcer: OnceLock<Arc<crate::voice_announce::VoiceAnnouncer>>,
    /// Ready gate — the scheduler skips `tick()` until this is true.
    /// Prevents jobs from firing before broadcast receivers (e.g. Slack)
    /// are fully initialised on startup; cleared again by [`Self::retire`].
    ready: Arc<AtomicBool>,
    /// The loop's shutdown token, stashed by [`Self::run`] so job arms can
    /// hand it to cancellable engines (the Claude Code consolidation run)
    /// and [`Self::retire`] can reap the loop.
    cancel: OnceLock<tokio_util::sync::CancellationToken>,
    /// Persistence backend, built lazily from `service.config` on first use
    /// (fail-closed: `load_jobs` propagates a backend that won't come up).
    store: tokio::sync::OnceCell<Arc<dyn crate::scheduler_store::JobStore>>,
    /// When the in-memory list was last refreshed from a shared store.
    last_store_refresh: std::sync::Mutex<Option<std::time::Instant>>,
    /// When the jobs a suspension holds were last checked for release.
    last_hold_review: std::sync::Mutex<Option<std::time::Instant>>,
    /// Held by a run in a pinned session, so a job fired by hand while its
    /// scheduled run is going waits for it instead of joining its session.
    /// Between instances the session's turn claim holds the same line.
    pinned_run: tokio::sync::Mutex<()>,
}

impl Scheduler {
    pub fn new(service: Arc<AgentService>) -> Self {
        Self::build(service, None, chrono_tz::UTC)
    }

    /// Create a scheduler with a specific default timezone.
    pub fn new_with_timezone(service: Arc<AgentService>, default_tz: Tz) -> Self {
        Self::build(service, None, default_tz)
    }

    /// Create a scheduler with a delivery callback and a specific default timezone.
    pub fn with_delivery_and_timezone(
        service: Arc<AgentService>,
        delivery: DeliveryCallback,
        default_tz: Tz,
    ) -> Self {
        Self::build(service, Some(delivery), default_tz)
    }

    fn build(seed: Arc<AgentService>, delivery: Option<DeliveryCallback>, default_tz: Tz) -> Self {
        // Per-agent job store under the agent's workspace, so each agent's
        // scheduler owns its own `jobs.json` and they never collide on names.
        let data_dir = seed.workspace.clone();
        Self {
            jobs: Arc::new(RwLock::new(Vec::new())),
            seed,
            owner: OnceLock::new(),
            delivery,
            default_tz,
            data_dir,
            run_history: Arc::new(RwLock::new(Vec::new())),
            max_history: 100,
            messaging_contexts: Arc::new(RwLock::new(HashMap::new())),
            leader: OnceLock::new(),
            voice_announcer: OnceLock::new(),
            ready: Arc::new(AtomicBool::new(false)),
            cancel: OnceLock::new(),
            store: tokio::sync::OnceCell::new(),
            last_store_refresh: std::sync::Mutex::new(None),
            last_hold_review: std::sync::Mutex::new(None),
            pinned_run: tokio::sync::Mutex::new(()),
        }
    }

    /// Adopt the scheduler into the service that registers its tools. Weak,
    /// since the owner holds the scheduler.
    pub fn set_owner(&self, owner: Weak<AgentService>) {
        if self.owner.set(owner).is_err() {
            warn!("Scheduler owner already set");
        }
    }

    /// Set the voice announcer so jobs created from a voice bridge can be
    /// spoken back on it. Same `&self`/`OnceLock` shape as the leader flag;
    /// never set in deployments without a gateway (tests, CLI).
    pub fn set_voice_announcer(&self, announcer: Arc<crate::voice_announce::VoiceAnnouncer>) {
        if self.voice_announcer.set(announcer).is_err() {
            warn!("Voice announcer already set");
        }
    }

    /// Signal that all messaging surfaces are ready and the scheduler may
    /// begin firing jobs. Until this is called, `tick()` is a no-op.
    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::SeqCst);
        info!("Scheduler ready — jobs will now fire");
    }

    /// Stop firing for good, ahead of the supervisor reaping the loop: a
    /// replacement scheduler shares this agent's job store, and two loops on
    /// one store double-fire and overwrite each other.
    pub fn retire(&self) {
        self.ready.store(false, Ordering::SeqCst);
        if let Some(cancel) = self.cancel.get() {
            cancel.cancel();
        }
        info!("Scheduler retired");
    }

    /// Set the leader flag. When set, the scheduler only executes jobs
    /// when this instance is the cluster leader.
    ///
    /// Takes `&self` (not `&mut self`) via `OnceLock`.
    pub fn set_leader_flag(&self, flag: Arc<AtomicBool>) {
        if self.leader.set(flag).is_err() {
            warn!("Leader flag already set");
        }
    }

    /// Set the default timezone used when evaluating cron expressions that
    /// don't carry their own `tz` field.
    pub fn set_default_timezone(&mut self, tz: Tz) {
        self.default_tz = tz;
    }

    /// Set the messaging context for a specific session (called before agent run).
    pub async fn set_messaging_context(
        &self,
        session_key: &str,
        channel_id: String,
        thread_id: Option<String>,
    ) {
        self.messaging_contexts
            .write()
            .await
            .insert(session_key.to_string(), (channel_id, thread_id));
    }

    /// Get the messaging context for a specific session.
    pub async fn get_messaging_context(
        &self,
        session_key: &str,
    ) -> Option<(String, Option<String>)> {
        self.messaging_contexts
            .read()
            .await
            .get(session_key)
            .cloned()
    }

    /// Clear the messaging context for a specific session (called after agent run completes).
    pub async fn clear_messaging_context(&self, session_key: &str) {
        self.messaging_contexts.write().await.remove(session_key);
    }

    /// Add a job (with optional description).
    pub async fn add_job(
        &self,
        name: &str,
        schedule: Schedule,
        payload: JobPayload,
    ) -> Result<String, String> {
        self.add_job_with_desc(name, None, schedule, payload).await
    }

    /// Add a job with a description. The creator is the principal of the
    /// turn in scope ([`creator_of`]) and the job defaults to its
    /// owner-derived visibility.
    pub async fn add_job_with_desc(
        &self,
        name: &str,
        description: Option<&str>,
        schedule: Schedule,
        payload: JobPayload,
    ) -> Result<String, String> {
        let creator = creator_of(
            Payer::classify(crate::user_config::current_identity_id().as_deref()),
            self.service().agent_id.as_str(),
        );
        self.insert_job(name, description, schedule, payload, creator, None)
            .await
    }

    /// Add a job with an explicit owner and visibility, for callers (e.g. the
    /// REST gateway) that resolve the principal themselves rather than through
    /// the `CURRENT_USER_ID` task-local.
    pub async fn add_job_owned(
        &self,
        name: &str,
        description: Option<&str>,
        schedule: Schedule,
        payload: JobPayload,
        creator_identity_id: Option<String>,
        visibility: Option<String>,
    ) -> Result<String, String> {
        self.insert_job(
            name,
            description,
            schedule,
            payload,
            creator_identity_id,
            visibility,
        )
        .await
    }

    /// The `[budget]` ceilings in force for this scheduler's agent.
    fn effective_budget(&self) -> workforce_common::settings::EffectiveBudget {
        let service = self.service();
        service.config.budget.for_agent(service.agent_id.as_str())
    }

    fn min_interval_secs(&self) -> u64 {
        let service = self.service();
        let budget = &service.config.budget;
        if budget.enabled {
            budget.min_recurring_interval_secs
        } else {
            0
        }
    }

    async fn insert_job(
        &self,
        name: &str,
        description: Option<&str>,
        schedule: Schedule,
        payload: JobPayload,
        creator_identity_id: Option<String>,
        visibility: Option<String>,
    ) -> Result<String, String> {
        let now = Utc::now();
        // Owned jobs (a human or agent principal created them) are floored;
        // creator-less registrations are code-owned cadences.
        if is_owned(creator_identity_id.as_deref()) {
            validate_schedule(&schedule, self.min_interval_secs(), &now, self.default_tz)?;
        }
        let next = next_run_time(&schedule, &now, self.default_tz);
        let is_reminder = matches!(payload, JobPayload::Reminder { .. });
        let caps = self.creation_caps(&schedule, &payload, creator_identity_id.as_deref(), now);
        let deliver_to = self.capture_voice_origin();
        let job = ScheduledJob {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            description: description.map(|s| s.to_string()),
            schedule,
            payload,
            enabled: true,
            created_at: now,
            last_run: None,
            next_run: next,
            run_count: 0,
            is_reminder,
            delivery_failures: 0,
            creator_identity_id,
            run_as_identity_id: None,
            bill_to_identity_id: None,
            visibility,
            max_runs: caps.0,
            budget_usd: caps.1,
            expires_at: caps.2,
            deliver_to,
            edited_by: automated_origin(),
            paused_by_suspension: false,
        };

        let id = job.id.clone();
        info!(job_id = %id, name = name, deliver_to = ?job.deliver_to, next_run = ?job.next_run, "Job added");
        let mut jobs = self.jobs.write().await;
        jobs.push(job.clone());
        self.persist_change(&jobs, crate::scheduler_store::JobChange::Upserted(&job))
            .await;
        Ok(id)
    }

    /// A schedule created during a spoken turn on a voice-bridge channel
    /// remembers that surface, so its result is spoken by the device the user
    /// asked from instead of defaulting to Slack.
    ///
    /// Reads the per-turn channel the gateway decided at dispatch — NOT the
    /// session's voice flag. One WebSocket anchor session is shared by every
    /// surface the same person uses with the same agent, so that flag answers
    /// "was one of my turns spoken", and a web turn racing a spoken one would
    /// be stamped for a device it never came from. The stamp is permanent and
    /// invisible, so it has to be right.
    ///
    /// Deliberately independent of whether a bridge is connected right now:
    /// the target is a standing fact about the job, and fire-time delivery
    /// handles absence by falling back to Slack.
    fn capture_voice_origin(&self) -> Option<DeliveryTarget> {
        workforce_agent::runner::CURRENT_VOICE_CHANNEL
            .try_with(|c| c.clone())
            .ok()
            .filter(|c| !c.is_empty())
            .map(|channel| DeliveryTarget {
                surface: DELIVERY_SURFACE_VOICE.to_string(),
                channel,
            })
    }

    /// Default run/spend/expiry caps for a new job. Only owned recurring
    /// agent-turn jobs are capped: system registrations and one-shots run
    /// uncapped, and reminders carry no LLM cost.
    fn creation_caps(
        &self,
        schedule: &Schedule,
        payload: &JobPayload,
        creator: Option<&str>,
        now: DateTime<Utc>,
    ) -> (Option<u64>, Option<f64>, Option<DateTime<Utc>>) {
        let capped = is_owned(creator)
            && self.service().config.budget.enabled
            && !matches!(schedule, Schedule::At { .. })
            && matches!(payload, JobPayload::AgentTurn { .. });
        if !capped {
            return (None, None, None);
        }
        let b = self.effective_budget();
        (
            (b.job_max_runs > 0).then_some(b.job_max_runs),
            (b.job_total_usd > 0.0).then_some(b.job_total_usd),
            (b.job_expiry_days > 0)
                .then(|| now + chrono::Duration::days(i64::from(b.job_expiry_days))),
        )
    }

    /// Add a job only if one with the same name doesn't already exist.
    /// Returns the existing job's ID if it's already registered.
    pub async fn ensure_job(
        &self,
        name: &str,
        description: Option<&str>,
        schedule: Schedule,
        payload: JobPayload,
    ) -> Result<String, String> {
        let jobs = self.jobs.read().await;
        if let Some(existing) = jobs.iter().find(|j| j.name == name && j.enabled) {
            info!(job_id = %existing.id, name = name, "Job already registered — skipping");
            return Ok(existing.id.clone());
        }
        drop(jobs);
        self.add_job_with_desc(name, description, schedule, payload)
            .await
    }

    /// Upsert a job by name: insert if absent, otherwise refresh `payload`,
    /// `schedule`, and `description` while preserving runtime state
    /// (`id`, `created_at`, `last_run`, `run_count`, etc.).
    ///
    /// Use this instead of `ensure_job` for jobs whose definition lives in
    /// code (e.g. memory consolidation): it keeps the persisted payload in
    /// sync with the latest code on every deploy, without losing run history.
    pub async fn upsert_job(
        &self,
        name: &str,
        description: Option<&str>,
        schedule: Schedule,
        payload: JobPayload,
    ) -> Result<String, String> {
        let now = Utc::now();
        let mut jobs = self.jobs.write().await;
        if let Some(existing) = jobs.iter_mut().find(|j| j.name == name) {
            if is_owned(existing.creator_identity_id.as_deref()) {
                validate_schedule(&schedule, self.min_interval_secs(), &now, self.default_tz)?;
            }
            existing.description = description.map(str::to_string);
            existing.payload = payload;
            // Re-arm only when the cadence actually changed. Recomputing on an
            // unchanged schedule skips the pending occurrence whenever a reload
            // lands between a job falling due and the tick firing it.
            let rearmed = existing.schedule != schedule;
            if rearmed {
                existing.next_run = next_run_time(&schedule, &now, self.default_tz);
                existing.schedule = schedule;
            }
            let id = existing.id.clone();
            let snapshot = existing.clone();
            info!(job_id = %id, name = name, "Job upserted — payload refreshed");
            // An unchanged cadence is a definition refresh, not a bookkeeping
            // write: on a shared store this copy of `next_run` may predate a
            // peer's tick (a booting follower syncs from its load-time list),
            // and writing it back re-arms an occurrence the leader already
            // fired — the 12 Sep 2026 double pr-review.
            let change = if rearmed {
                crate::scheduler_store::JobChange::Upserted(&snapshot)
            } else {
                crate::scheduler_store::JobChange::Refreshed(&snapshot)
            };
            self.persist_change(&jobs, change).await;
            return Ok(id);
        }
        drop(jobs);
        self.add_job_with_desc(name, description, schedule, payload)
            .await
    }

    /// Remove every persisted WorkflowStart job from this scheduler.
    ///
    /// The workflow runtime arms schedule triggers on exactly one scheduler —
    /// its own service's — and its stale-sweep only sees that store. A
    /// WorkflowStart job persisted on any *other* agent's store is a stale
    /// survivor of the per-agent-runtime era; left in place it fires
    /// duplicate instances of every scheduled workflow.
    pub async fn remove_workflow_start_jobs(&self) -> usize {
        let mut jobs = self.jobs.write().await;
        let ids: Vec<String> = jobs
            .iter()
            .filter(|j| matches!(j.payload, JobPayload::WorkflowStart { .. }))
            .map(|j| j.id.clone())
            .collect();
        if ids.is_empty() {
            return 0;
        }
        jobs.retain(|j| !matches!(j.payload, JobPayload::WorkflowStart { .. }));
        self.persist_change(&jobs, crate::scheduler_store::JobChange::Removed(&ids))
            .await;
        ids.len()
    }

    /// Remove a job.
    pub async fn remove_job(&self, job_id: &str) -> bool {
        let mut jobs = self.jobs.write().await;
        let before = jobs.len();
        jobs.retain(|j| j.id != job_id);
        let removed = jobs.len() < before;
        if removed {
            let ids = [job_id.to_string()];
            self.persist_change(&jobs, crate::scheduler_store::JobChange::Removed(&ids))
                .await;
        }
        removed
    }

    /// List all jobs.
    pub async fn list_jobs(&self) -> Vec<ScheduledJob> {
        self.jobs.read().await.clone()
    }

    /// Get a job by ID.
    pub async fn get_job(&self, job_id: &str) -> Option<ScheduledJob> {
        self.jobs
            .read()
            .await
            .iter()
            .find(|j| j.id == job_id)
            .cloned()
    }

    /// Trigger a job to run immediately (regardless of schedule).
    pub async fn run_job_now(&self, job_id: &str) -> Option<JobRunRecord> {
        let job = self.get_job(job_id).await?;
        let record = Box::pin(self.execute_job(&job)).await;
        Some(record)
    }

    /// Get run history.
    pub async fn run_history(&self) -> Vec<JobRunRecord> {
        self.run_history.read().await.clone()
    }

    /// Whether a job on `schedule` may keep one session across its runs: a
    /// one-shot always, a recurring job while the agent's `[scheduler]`
    /// config allows it.
    pub fn may_pin(&self, schedule: &Schedule) -> bool {
        matches!(schedule, Schedule::At { .. }) || self.service().scheduler_policy().pinned_sessions
    }

    /// What a job on `schedule` running `model` (the agent's own when unset)
    /// is forecast to cost, keeping one session when `pins`. `None` for a
    /// one-shot and where budgets are off: neither is confirmed.
    pub async fn recurring_cost(
        &self,
        schedule: &Schedule,
        model: Option<&str>,
        pins: bool,
    ) -> Option<RecurringCost> {
        let service = self.service();
        let budget = &service.config.budget;
        if !budget.enabled || matches!(schedule, Schedule::At { .. }) {
            return None;
        }
        let model = model
            .map(str::to_string)
            .or_else(|| service.active_model.read().clone())
            .unwrap_or_else(|| service.config.llm.default_model.clone());
        let per_run = crate::cost_forecast::CostForecaster::new(
            Arc::clone(&service.telemetry_store),
            budget.warn_turn_usd,
        )
        .forecast(service.agent_id.as_str(), None, &model)
        .await
        .unwrap_or_else(|_| crate::cost_forecast::CostForecast::default_estimate());
        let projected = per_run.recurring(runs_per_day(schedule, &Utc::now(), self.default_tz));
        Some(RecurringCost {
            pins,
            per_run,
            projected,
            threshold_usd: budget.confirm_recurring_daily_usd,
        })
    }

    /// The approval `edit` needs before it is applied to `job`: it pins a
    /// recurring job, or makes one run more often at a projected cost over
    /// the confirmation threshold. What create asks, asked of a change.
    async fn confirmation_for(&self, job: &ScheduledJob, edit: &JobEdit) -> Option<RecurringCost> {
        let after = edit.schedule.as_ref().unwrap_or(&job.schedule);
        let per_day = |schedule| runs_per_day(schedule, &Utc::now(), self.default_tz);
        let pins = edit.pins(job);
        if !pins && per_day(after) <= per_day(&job.schedule) {
            return None;
        }
        let model = match &job.payload {
            JobPayload::AgentTurn { model, .. } => model.as_deref(),
            _ => None,
        };
        self.recurring_cost(after, model, pins)
            .await
            .filter(RecurringCost::needs_confirmation)
    }

    /// Whether `job` can take `edit`.
    fn check_edit(
        &self,
        job: &ScheduledJob,
        edit: &JobEdit,
        now: &DateTime<Utc>,
    ) -> Result<(), String> {
        if let Some(ref s) = edit.schedule {
            if is_owned(job.creator_identity_id.as_deref()) {
                validate_schedule(s, self.min_interval_secs(), now, self.default_tz)?;
            }
        }
        match edit.visibility.as_deref() {
            None | Some(VISIBILITY_PRIVATE) => {}
            Some(VISIBILITY_GLOBAL) if !job.is_reminder => {}
            Some(VISIBILITY_GLOBAL) => return Err("a reminder is always private".to_string()),
            Some(other) => {
                return Err(format!(
                "unknown visibility '{other}': use '{VISIBILITY_PRIVATE}' or '{VISIBILITY_GLOBAL}'"
            ))
            }
        }
        if (edit.session.is_some() || edit.tools.is_some())
            && !matches!(job.payload, JobPayload::AgentTurn { .. })
        {
            return Err("only a job that runs an agent turn has a session or a tool list".into());
        }
        if edit.pins(job) && !self.may_pin(edit.schedule.as_ref().unwrap_or(&job.schedule)) {
            return Err(
                "this agent's config does not let recurring jobs pin a session".to_string(),
            );
        }
        Ok(())
    }

    /// Apply `edit` to a job in a single pass. Returns the updated job, or
    /// `None` if the job was not found; nothing is applied when any part of
    /// the edit is refused, or while it makes the job cost more and is not
    /// `confirmed`. Pinning gives the job a session of its own (a caller
    /// never names one), and a job already pinned keeps the one it has. The
    /// edit is applied to the job as the store holds it and writes only the
    /// fields it changes, so it never takes back what a peer wrote.
    ///
    /// It is saved as `editor`, or as the automated turn in scope
    /// ([`automated_origin`]) whoever that runs as. An edit that changes what
    /// the job does or who may read it records the editor as the job's last
    /// ([`ScheduledJob::edited_by`]); any save by its creator records them,
    /// so the job is theirs alone again.
    pub async fn update_job(
        &self,
        job_id: &str,
        edit: JobEdit,
        editor: &str,
    ) -> Result<Option<ScheduledJob>, EditRefused> {
        let now = Utc::now();
        let Some(asked_of) = self.get_job(job_id).await else {
            return Ok(None);
        };
        self.check_edit(&asked_of, &edit, &now)
            .map_err(EditRefused::Invalid)?;
        if !edit.confirmed {
            if let Some(cost) = self.confirmation_for(&asked_of, &edit).await {
                return Err(EditRefused::NeedsConfirmation(cost));
            }
        }
        let mut jobs = self.jobs.write().await;
        self.reload_from_shared_store(&mut jobs).await;
        let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) else {
            return Ok(None);
        };
        self.check_edit(job, &edit, &now)
            .map_err(EditRefused::Invalid)?;
        let editor = automated_origin().unwrap_or_else(|| editor.to_string());
        let mut fields = edit.fields();
        if edit.changes_control() || job.creator_identity_id.as_deref() == Some(editor.as_str()) {
            job.edited_by = Some(editor);
            fields.push("edited_by");
        }
        edit.apply(job, &now, self.default_tz);
        let updated = job.clone();
        self.persist_change(
            &jobs,
            crate::scheduler_store::JobChange::Fields(&updated, &fields),
        )
        .await;
        drop(jobs);
        info!(job_id = %job_id, "Job updated");
        Ok(Some(updated))
    }

    /// What `job_id`'s runs have cost so far, in USD. `None` when the cost
    /// store cannot say.
    pub fn job_spend(&self, job_id: &str) -> Option<f64> {
        self.service()
            .telemetry_store
            .metered_cost_by_task_prefix(&format!("scheduler:{job_id}:"))
            .inspect_err(|e| warn!(job_id = %job_id, error = %e, "Job spend lookup failed"))
            .ok()
    }

    /// The agent service this scheduler runs jobs against: the owner once
    /// adopted, the seed before.
    pub fn service(&self) -> Arc<AgentService> {
        self.owner
            .get()
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| Arc::clone(&self.seed))
    }

    /// Tighten a job's caps: each `Some(v)` lowers the field to `min(existing,
    /// v)` (an unset field takes `v`). Caps can never be loosened this way.
    pub async fn tighten_job_caps(
        &self,
        job_id: &str,
        max_runs: Option<u64>,
        budget_usd: Option<f64>,
        expires_at: Option<DateTime<Utc>>,
    ) -> bool {
        let mut jobs = self.jobs.write().await;
        let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) else {
            return false;
        };
        if let Some(v) = max_runs {
            job.max_runs = Some(job.max_runs.map_or(v, |cur| cur.min(v)));
        }
        if let Some(v) = budget_usd {
            job.budget_usd = Some(job.budget_usd.map_or(v, |cur| cur.min(v)));
        }
        if let Some(v) = expires_at {
            job.expires_at = Some(job.expires_at.map_or(v, |cur| cur.min(v)));
        }
        let snapshot = job.clone();
        self.persist_change(
            &jobs,
            crate::scheduler_store::JobChange::Fields(
                &snapshot,
                &["max_runs", "budget_usd", "expires_at"],
            ),
        )
        .await;
        true
    }

    /// Every job, read fresh from the shared store.
    pub async fn current_jobs(&self) -> Vec<ScheduledJob> {
        let mut jobs = self.jobs.write().await;
        self.reload_from_shared_store(&mut jobs).await;
        jobs.clone()
    }

    /// Every job that acts for, bills or was last saved by anyone in
    /// `names`, read fresh from the shared store.
    pub async fn jobs_naming(&self, names: &[String]) -> Vec<ScheduledJob> {
        self.current_jobs()
            .await
            .into_iter()
            .filter(|job| job.names_any(names))
            .collect()
    }

    /// Make every identity field of the jobs `ids` (all of them when `None`)
    /// that names anyone in `names` name `to`. With `editor`, an
    /// administrator moved the job to someone else and it counts as saved by
    /// them ([`ScheduledJob::edited_by`]); without, the same person now goes
    /// by `to`, so an edit of theirs stays theirs. Reloads from the shared
    /// store and writes only the fields it changes, so a peer's bookkeeping
    /// is never overwritten. Returns the ids changed.
    pub async fn reassign_jobs(
        &self,
        ids: Option<&[String]>,
        names: &[String],
        to: &str,
        editor: Option<&str>,
    ) -> Vec<String> {
        let mut jobs = self.jobs.write().await;
        self.reload_from_shared_store(&mut jobs).await;
        let mut changed = Vec::new();
        for job in jobs
            .iter_mut()
            .filter(|job| ids.is_none_or(|ids| ids.contains(&job.id)))
        {
            let mut fields = job.reassign(names, to);
            if let (Some(editor), false) = (editor, fields.is_empty()) {
                job.edited_by = Some(editor.to_string());
                fields.push("edited_by");
            }
            if !fields.is_empty() {
                changed.push((job.clone(), fields));
            }
        }
        for (job, fields) in &changed {
            self.persist_change(
                &jobs,
                crate::scheduler_store::JobChange::Fields(job, fields),
            )
            .await;
        }
        drop(jobs);
        changed.into_iter().map(|(job, _)| job.id).collect()
    }

    /// Stop every job someone else owns from running as or billing anyone in
    /// `names` (who are being deleted): such a job runs as and bills its
    /// owner again. Who last edited it stays, as a record. Returns the ids
    /// changed.
    pub async fn release_jobs(&self, names: &[String]) -> Vec<String> {
        let mut jobs = self.jobs.write().await;
        self.reload_from_shared_store(&mut jobs).await;
        let named = |id: &Option<String>| id.as_ref().is_some_and(|id| names.contains(id));
        let mut changed = Vec::new();
        for job in jobs.iter_mut().filter(|job| !job.owned_by(names)) {
            let mut fields = Vec::new();
            if named(&job.run_as_identity_id) {
                job.run_as_identity_id = None;
                fields.push("run_as_identity_id");
            }
            if named(&job.bill_to_identity_id) {
                job.bill_to_identity_id = None;
                fields.push("bill_to_identity_id");
            }
            if !fields.is_empty() {
                changed.push((job.clone(), fields));
            }
        }
        for (job, fields) in &changed {
            self.persist_change(
                &jobs,
                crate::scheduler_store::JobChange::Fields(job, fields),
            )
            .await;
        }
        drop(jobs);
        changed.into_iter().map(|(job, _)| job.id).collect()
    }

    /// Hold every enabled job run for anyone in `names`, who is suspended
    /// ([`ScheduledJob::held_for`]). With `apply` off, only say which.
    pub async fn hold_for_suspension(&self, names: &[String], apply: bool) -> Vec<ScheduledJob> {
        self.rewrite(apply, "enabled", |job| {
            (job.enabled && job.held_for().is_some_and(|p| names.iter().any(|n| n == p)))
                .then(|| job.hold())
        })
        .await
    }

    /// Release every job a suspension holds whose person `suspended` no
    /// longer says is, and only those: one someone switched on or off
    /// since is theirs to decide and is no longer held. With `apply` off,
    /// only say which.
    pub async fn release_suspension_holds(
        &self,
        suspended: &(dyn Fn(&str) -> bool + Send + Sync),
        apply: bool,
    ) -> Vec<ScheduledJob> {
        let now = Utc::now();
        self.rewrite(apply, "paused_by_suspension", |job| {
            (job.paused_by_suspension && !job.held_for().is_some_and(suspended))
                .then(|| job.release(&now, self.default_tz))
        })
        .await
    }

    /// Apply `change` to every job, read fresh from the shared store, and
    /// write only the fields it names for each job it changed, and only
    /// while the stored job's `while_set` is still true, so a peer's change
    /// since the read is not overwritten. With `apply` off nothing is kept.
    /// Returns the changed jobs.
    async fn rewrite(
        &self,
        apply: bool,
        while_set: &'static str,
        mut change: impl FnMut(&mut ScheduledJob) -> Option<&'static [&'static str]>,
    ) -> Vec<ScheduledJob> {
        let mut jobs = self.jobs.write().await;
        self.reload_from_shared_store(&mut jobs).await;
        let changed: Vec<(ScheduledJob, &'static [&'static str])> = match apply {
            true => jobs
                .iter_mut()
                .filter_map(|job| change(job).map(|fields| (job.clone(), fields)))
                .collect(),
            false => jobs
                .iter()
                .filter_map(|job| {
                    let mut job = job.clone();
                    change(&mut job).map(|fields| (job, fields))
                })
                .collect(),
        };
        if apply {
            for (job, fields) in &changed {
                self.persist_change(
                    &jobs,
                    crate::scheduler_store::JobChange::FieldsWhile(job, fields, while_set),
                )
                .await;
            }
        }
        changed.into_iter().map(|(job, _)| job).collect()
    }

    /// Remove the jobs `ids` and every run of theirs this hub recorded: a
    /// run's output is the person's. Returns how many jobs went.
    pub async fn forget_jobs(&self, ids: &[String]) -> usize {
        let removed = {
            let mut jobs = self.jobs.write().await;
            self.reload_from_shared_store(&mut jobs).await;
            let before = jobs.len();
            jobs.retain(|job| !ids.contains(&job.id));
            let removed = before - jobs.len();
            if removed > 0 {
                self.persist_change(&jobs, crate::scheduler_store::JobChange::Removed(ids))
                    .await;
            }
            removed
        };
        let mut history = self.run_history.write().await;
        history.retain(|run| !ids.contains(&run.job_id));
        self.rewrite_run_history(|run| !ids.contains(&run.job_id));
        removed
    }

    /// Rewrite the persisted run history keeping the runs `keep` admits.
    /// Called with the history lock held, which [`Self::push_history`] also
    /// takes before it appends, so no run is lost to the rewrite.
    fn rewrite_run_history(&self, keep: impl Fn(&JobRunRecord) -> bool) {
        let path = self.data_dir.join("scheduler").join("run-history.jsonl");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return;
        };
        let kept: String = raw
            .lines()
            .filter(|line| {
                serde_json::from_str::<JobRunRecord>(line).map_or(true, |run| keep(&run))
            })
            .map(|line| format!("{line}\n"))
            .collect();
        let staged = path.with_extension("jsonl.rewrite");
        let written = workforce_common::private_fs::private_file_options()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&staged)
            .and_then(|mut file| std::io::Write::write_all(&mut file, kept.as_bytes()))
            .and_then(|()| std::fs::rename(&staged, &path));
        if let Err(e) = written {
            warn!(error = %e, path = %path.display(), "Rewriting scheduler run history failed");
        }
    }

    /// Set the creator identity on a job (for per-user scheduled tasks).
    /// Returns `true` if the job was found and updated.
    pub async fn update_job_identity(&self, job_id: &str, identity_id: Option<String>) -> bool {
        let mut jobs = self.jobs.write().await;
        let snapshot = if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
            job.creator_identity_id = identity_id;
            Some(job.clone())
        } else {
            warn!(job_id = %job_id, "Cannot set identity: job not found");
            None
        };
        match snapshot {
            Some(job) => {
                self.persist_change(
                    &jobs,
                    crate::scheduler_store::JobChange::Fields(&job, &["creator_identity_id"]),
                )
                .await;
                true
            }
            None => false,
        }
    }

    /// Set the `run_as_identity_id` override on a job. Takes precedence over
    /// `creator_identity_id` when scoping `CURRENT_USER_ID` at job execution.
    pub async fn update_job_run_as(&self, job_id: &str, identity_id: Option<String>) -> bool {
        let mut jobs = self.jobs.write().await;
        let snapshot = if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
            job.run_as_identity_id = identity_id;
            Some(job.clone())
        } else {
            warn!(job_id = %job_id, "Cannot set run_as: job not found");
            None
        };
        match snapshot {
            Some(job) => {
                self.persist_change(
                    &jobs,
                    crate::scheduler_store::JobChange::Fields(&job, &["run_as_identity_id"]),
                )
                .await;
                true
            }
            None => false,
        }
    }

    /// Set (or clear) the identity this job's spend is billed to. Access is
    /// unchanged — the job still runs as `effective_run_as_identity`; only
    /// budget admission and cost attribution move. Returns `true` if found.
    pub async fn update_job_bill_to(&self, job_id: &str, identity_id: Option<String>) -> bool {
        let mut jobs = self.jobs.write().await;
        let snapshot = if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
            job.bill_to_identity_id = identity_id;
            Some(job.clone())
        } else {
            warn!(job_id = %job_id, "Cannot set bill_to: job not found");
            None
        };
        match snapshot {
            Some(job) => {
                self.persist_change(
                    &jobs,
                    crate::scheduler_store::JobChange::Fields(&job, &["bill_to_identity_id"]),
                )
                .await;
                true
            }
            None => false,
        }
    }

    /// How the latest `n` runs of `job_id` this hub holds went.
    pub async fn recent_outcomes(&self, job_id: &str, n: usize) -> Outcomes {
        outcomes(
            self.run_history
                .read()
                .await
                .iter()
                .filter(|run| run.job_id == job_id),
            n,
        )
    }

    /// Get run history for a specific job.
    pub async fn job_run_history(&self, job_id: &str) -> Vec<JobRunRecord> {
        self.run_history
            .read()
            .await
            .iter()
            .filter(|r| r.job_id == job_id)
            .cloned()
            .collect()
    }

    /// Load persisted jobs from the configured store. Fail-closed: a backend
    /// that won't come up (or a corrupt file) is an error, not a silent
    /// empty job list — persisting over one would lose every job.
    pub async fn load_jobs(&self) -> eyre::Result<()> {
        let store = self.job_store().await?;
        let mut jobs = store.load_all().await?;

        // One-time backfill: owned recurring agent jobs persisted before the
        // caps existed would otherwise run bounded only by the daily scopes.
        let mut backfilled = 0usize;
        for job in jobs
            .iter_mut()
            .filter(|j| j.max_runs.is_none() && j.budget_usd.is_none() && j.expires_at.is_none())
        {
            let (max_runs, budget_usd, expires_at) = self.creation_caps(
                &job.schedule,
                &job.payload,
                job.creator_identity_id.as_deref(),
                Utc::now(),
            );
            if max_runs.is_some() || budget_usd.is_some() || expires_at.is_some() {
                job.max_runs = max_runs;
                job.budget_usd = budget_usd;
                job.expires_at = expires_at;
                backfilled += 1;
            }
        }
        let count = jobs.len();
        let mut guard = self.jobs.write().await;
        *guard = jobs;
        if backfilled > 0 {
            info!(backfilled, "Applied budget caps to pre-existing owned jobs");
            self.persist_change(&guard, crate::scheduler_store::JobChange::Replaced)
                .await;
        }
        drop(guard);
        if count > 0 {
            info!(count = count, "Loaded persisted scheduler jobs");
        }
        Ok(())
    }

    /// Run the scheduler tick loop.
    ///
    /// Stops gracefully when `cancel` is triggered.
    pub async fn run(self: Arc<Self>, cancel: tokio_util::sync::CancellationToken) {
        info!("Scheduler started");
        let _ = self.cancel.set(cancel.clone());
        // Jobs are loaded before built-in registration in serve.rs — no load here.

        loop {
            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
                    self.tick().await;
                }
                _ = cancel.cancelled() => {
                    info!("Scheduler stopped (leadership lost or shutdown)");
                    return;
                }
            }
        }
    }

    /// Check and execute due jobs.
    async fn tick(&self) {
        // Don't fire jobs before messaging surfaces are ready or after retirement.
        if !self.ready.load(Ordering::SeqCst) {
            return;
        }

        // Before the leader gate, so follower API reads stay fresh too.
        self.refresh_from_shared_store().await;

        // In multi-instance mode, only the leader executes jobs.
        if let Some(flag) = self.leader.get() {
            if !flag.load(Ordering::Relaxed) {
                return;
            }
        }

        let holding = self
            .jobs
            .read()
            .await
            .iter()
            .any(|job| job.paused_by_suspension);
        if holding
            && refresh_due(
                &mut self
                    .last_hold_review
                    .lock()
                    .expect("hold review mutex poisoned"),
                HOLD_REVIEW_INTERVAL,
            )
        {
            let service = self.service();
            for job in self
                .release_suspension_holds(&|person| service.suspended(person).is_some(), true)
                .await
            {
                info!(job_id = %job.id, name = %job.name, "Scheduled job released: the person it runs for is no longer suspended");
            }
        }

        let now = Utc::now();
        let mut to_run = Vec::new();
        let mut breached = Vec::new();
        let mut held = Vec::new();

        {
            let service = self.service();
            let jobs = self.jobs.read().await;
            for job in jobs.iter() {
                if !job.enabled {
                    continue;
                }
                let Some(next) = job.next_run else { continue };
                if next > now {
                    continue;
                }
                if job
                    .held_for()
                    .is_some_and(|person| service.suspended(person).is_some())
                {
                    held.push(job.id.clone());
                    continue;
                }
                match self.cap_breach(job, &now) {
                    Some(reason) => breached.push((job.id.clone(), reason)),
                    None => to_run.push(job.clone()),
                }
            }
        }

        if !held.is_empty() {
            for job in self
                .rewrite(true, "enabled", |job| {
                    (job.enabled && held.contains(&job.id)).then(|| job.hold())
                })
                .await
            {
                info!(job_id = %job.id, name = %job.name, "Scheduled job held: the person it runs for is suspended");
            }
        }

        for (job_id, reason) in breached {
            self.disable_with_notice(&job_id, &reason).await;
        }

        for job in to_run {
            let record = Box::pin(self.execute_job(&job)).await;
            let delivered = record.delivered;
            let succeeded = record.success;

            // Add to run history
            self.push_history(record).await;

            // Update job state
            let mut disabled: Option<(ScheduledJob, String)> = None;
            let mut bookkept: Option<ScheduledJob> = None;
            let mut jobs = self.jobs.write().await;
            if let Some(j) = jobs.iter_mut().find(|j| j.id == job.id) {
                j.last_run = Some(now);
                j.run_count += 1;

                if !delivered && j.is_reminder {
                    // Delivery failed — schedule retry with exponential backoff
                    j.delivery_failures += 1;
                    if j.delivery_failures <= MAX_CONSECUTIVE_FAILURES {
                        let backoff_secs = 30i64 * 2i64.pow(j.delivery_failures.saturating_sub(1));
                        j.next_run = Some(now + chrono::Duration::seconds(backoff_secs));
                        warn!(
                            job_id = %j.id,
                            attempt = j.delivery_failures,
                            next_retry_secs = backoff_secs,
                            "Reminder delivery failed — scheduling retry"
                        );
                    } else {
                        j.enabled = false;
                        error!(
                            job_id = %j.id,
                            name = %j.name,
                            "Reminder delivery failed after {} attempts — giving up",
                            MAX_CONSECUTIVE_FAILURES
                        );
                    }
                } else {
                    // A failing agent job previously retried forever (reminders
                    // were the only self-disabling kind); it now shares the
                    // consecutive-failure counter. Agent turns are the only
                    // non-reminder payload with an expected delivery, so an
                    // undeliverable one (deleted channel) counts as a failure
                    // instead of burning full turn cost until its caps.
                    let delivery_expected = matches!(j.payload, JobPayload::AgentTurn { .. });
                    let run_ok = succeeded && (delivered || !delivery_expected);
                    if j.is_reminder || run_ok {
                        j.delivery_failures = 0;
                    } else {
                        j.delivery_failures += 1;
                        if j.delivery_failures > MAX_CONSECUTIVE_FAILURES {
                            j.enabled = false;
                            disabled = Some((
                                j.clone(),
                                format!("it failed {} runs in a row", j.delivery_failures),
                            ));
                        }
                    }
                    j.next_run = next_run_time(&j.schedule, &now, self.default_tz);

                    // Disable one-shot jobs after successful delivery
                    if matches!(j.schedule, Schedule::At { .. }) {
                        j.enabled = false;
                    }

                    if j.enabled {
                        if let Some(max) = j.max_runs.filter(|m| j.run_count >= *m) {
                            j.enabled = false;
                            disabled =
                                Some((j.clone(), format!("it reached its run limit of {max}")));
                        }
                    }
                }
                bookkept = Some(j.clone());
            }
            if let Some(job) = bookkept {
                let written = RUN_BOOKKEEPING.len() - usize::from(job.enabled);
                self.persist_change(
                    &jobs,
                    crate::scheduler_store::JobChange::Fields(&job, &RUN_BOOKKEEPING[..written]),
                )
                .await;
            }
            drop(jobs);
            if let Some((j, reason)) = disabled {
                self.notify_owner_disabled(&j, &reason).await;
            }
        }

        // Clean up disabled one-shot jobs older than 24 hours
        let cutoff = now - chrono::Duration::hours(24);
        let keep = |j: &ScheduledJob| {
            j.enabled
                || j.paused_by_suspension
                || !matches!(j.schedule, Schedule::At { .. })
                || j.last_run.is_none_or(|lr| lr > cutoff)
        };
        let mut jobs = self.jobs.write().await;
        let removed_ids: Vec<String> = jobs
            .iter()
            .filter(|j| !keep(j))
            .map(|j| j.id.clone())
            .collect();
        jobs.retain(|j| keep(j));
        if !removed_ids.is_empty() {
            info!(removed = removed_ids.len(), "Cleaned up old one-shot jobs");
            self.persist_change(
                &jobs,
                crate::scheduler_store::JobChange::Removed(&removed_ids),
            )
            .await;
        }
    }

    /// The reason a due job may not fire, if any of its caps are breached.
    /// Budget lookups fail open (a broken telemetry store must not wedge the
    /// scheduler) and only run for jobs that carry a `budget_usd`.
    fn cap_breach(&self, job: &ScheduledJob, now: &DateTime<Utc>) -> Option<String> {
        if let Some(expires) = job.expires_at.filter(|e| e <= now) {
            return Some(format!(
                "it expired on {}",
                expires.format("%Y-%m-%d %H:%M UTC")
            ));
        }
        if let Some(max) = job.max_runs.filter(|m| job.run_count >= *m) {
            return Some(format!("it reached its run limit of {max}"));
        }
        let budget = job.budget_usd.filter(|b| *b > 0.0)?;
        self.job_spend(&job.id)
            .filter(|spent| *spent >= budget)
            .map(|spent| format!("it reached its ${budget:.2} budget (${spent:.2} spent)"))
    }

    async fn disable_with_notice(&self, job_id: &str, reason: &str) {
        let mut jobs = self.jobs.write().await;
        let Some(j) = jobs.iter_mut().find(|j| j.id == job_id) else {
            return;
        };
        j.enabled = false;
        let job = j.clone();
        self.persist_change(
            &jobs,
            crate::scheduler_store::JobChange::Fields(&job, &["enabled"]),
        )
        .await;
        drop(jobs);
        self.notify_owner_disabled(&job, reason).await;
    }

    /// Best-effort DM to the job's creator explaining an auto-disable.
    async fn notify_owner_disabled(&self, job: &ScheduledJob, reason: &str) {
        warn!(job_id = %job.id, name = %job.name, reason = reason, "Scheduled job auto-disabled");
        let Some(owner) = job.creator_identity_id.as_deref() else {
            return;
        };
        let Some(slack_uid) = self.slack_user_for_dm(owner) else {
            return;
        };
        let service = self.service();
        let registry = service.surface_registry.read().await;
        let Ok((_, dm_channel)) = registry
            .resolve_user_dm(
                "slack",
                &slack_uid,
                workforce_common::channels::ChannelHint::default(),
            )
            .await
        else {
            return;
        };
        let text = format!(
            "⏸️ Your scheduled job *{}* was disabled because {}. \
             Recreate it with a narrower scope (longer interval, explicit \
             `max_runs`, or an expiry) if it is still needed.",
            job.name, reason
        );
        for surface in registry.all() {
            let _ = surface
                .send_message(&OutboundMessage::text(dm_channel.clone(), text.clone()))
                .await;
        }
    }

    /// Map a job's stored user id — the internal identity UUID — to the Slack
    /// id `conversations.open` needs. Non-UUID ids pass through unchanged
    /// (they are already platform ids); `None` means the identity has no
    /// Slack link, so a DM cannot be opened and callers must fall back to the
    /// stored channel without a doomed Slack call.
    fn slack_user_for_dm(&self, user_id: &str) -> Option<String> {
        Some(self.service().resolve_platform_user_id(user_id)).filter(|s| !s.is_empty())
    }

    /// Try to speak `text` on the job's voice target. `false` means nothing
    /// is listening (or no announcer is wired) and the caller should fall
    /// back to Slack. Announcements are scoped to the job's owner:
    /// `CURRENT_USER_ID` is the run-as identity for the whole of
    /// `execute_job`, and the gateway refuses to forward one user's content
    /// to another user's device.
    async fn deliver_via_voice(&self, job: &ScheduledJob, text: &str) -> bool {
        let Some(target) = job.deliver_to.as_ref().filter(|t| t.is_voice()) else {
            return false;
        };
        let Some(announcer) = self.voice_announcer.get() else {
            return false;
        };
        // An announcement is always somebody's. A job whose owner cannot be
        // resolved has nobody to speak it to, and delivering it to whichever
        // device happens to hold the channel would be a cross-user leak — so
        // it takes the Slack path like any other ownerless job.
        let Some(owner) = workforce_agent::runner::CURRENT_USER_ID
            .try_with(|u| u.clone())
            .ok()
            .filter(|u| !u.is_empty())
        else {
            warn!(
                job_id = %job.id,
                "voice-targeted job has no owner identity — falling back to Slack"
            );
            return false;
        };
        match announcer
            .announce(
                &target.channel,
                &owner,
                &crate::formatting::normalize_for_tts(text),
            )
            .await
        {
            Ok(()) => {
                info!(
                    job_id = %job.id,
                    channel = %target.channel,
                    "Delivered via voice announcement"
                );
                true
            }
            Err(e) => {
                warn!(
                    job_id = %job.id,
                    channel = %target.channel,
                    error = %e,
                    "Voice delivery unavailable — falling back to Slack"
                );
                false
            }
        }
    }

    /// Whom a job's spend is billed to: its explicit bill-to, else, when no
    /// person runs it, the person its background work falls to.
    fn billing_of(&self, job: &ScheduledJob) -> Option<String> {
        job.bill_to_identity_id
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.service()
                    .background_principal(job.effective_run_as_identity())
                    .bill_to
            })
    }

    /// Execute a job and return a run record.
    ///
    /// If the job has a `run_as_identity_id` or `creator_identity_id`, scopes
    /// `CURRENT_USER_ID` for the duration so credential resolution and memory
    /// scoping use the correct identity. Jobs without an identity (legacy or
    /// system-created) run without user context. The job is billed as
    /// [`Self::billing_of`] says.
    async fn execute_job(&self, job: &ScheduledJob) -> JobRunRecord {
        let billing = self.billing_of(job).unwrap_or_default();
        // Billing scoped even when empty: an empty value is treated as unset
        // by `current_billing_identity_id`, so nesting stays uniform.
        match job.effective_run_as_identity() {
            Some(id) => {
                workforce_agent::runner::CURRENT_USER_ID
                    .scope(
                        id.to_string(),
                        workforce_agent::runner::CURRENT_BILLING_IDENTITY
                            .scope(billing, self.execute_job_inner(job)),
                    )
                    .await
            }
            None => {
                workforce_agent::runner::CURRENT_BILLING_IDENTITY
                    .scope(billing, self.execute_job_inner(job))
                    .await
            }
        }
    }

    async fn execute_job_inner(&self, job: &ScheduledJob) -> JobRunRecord {
        info!(job_id = %job.id, name = %job.name, "Executing scheduled job");
        let started_at = Utc::now();
        let start = std::time::Instant::now();
        let service = self.service();
        let kept = job.pinned_session();
        let note = (kept.is_some() && !self.may_pin(&job.schedule)).then_some(RAN_FRESH_BY_POLICY);
        let session = kept.filter(|_| note.is_none());

        let (success, output, delivered) = match &job.payload {
            JobPayload::SystemEvent {
                text,
                channel_id,
                thread_id: _,
                user_id,
            } => {
                info!(job_id = %job.id, name = %job.name, "Delivering system event");
                let mut delivered = false;

                // Always deliver to DM via open_conversation for reliability.
                // Stored channel_id/thread_id can become stale after token
                // rotation or thread deletion.
                let registry = service.surface_registry.read().await;

                let slack_uid = user_id.as_ref().and_then(|uid| {
                    let mapped = self.slack_user_for_dm(uid);
                    if mapped.is_none() {
                        warn!(
                            job_id = %job.id,
                            user_id = %uid,
                            "System-event owner has no linked Slack id — using stored channel"
                        );
                    }
                    mapped
                });
                if let Some(uid) = slack_uid {
                    match registry
                        .resolve_user_dm(
                            "slack",
                            &uid,
                            workforce_common::channels::ChannelHint::default(),
                        )
                        .await
                    {
                        Ok((surface, dm_channel)) => {
                            info!(
                                job_id = %job.id,
                                dm_channel = %dm_channel,
                                "Delivering system event to DM"
                            );
                            match surface
                                .send_message(&OutboundMessage {
                                    channel: dm_channel,
                                    text: text.clone(),
                                    thread_id: None,
                                    username: None,
                                    icon_url: None,
                                    blocks: None,
                                })
                                .await
                            {
                                Ok(_) => delivered = true,
                                Err(e) => {
                                    error!(
                                        job_id = %job.id,
                                        error = %e,
                                        "Failed to deliver system event"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            error!(
                                job_id = %job.id,
                                user_id = %uid,
                                error = %e,
                                "Failed to open DM for system event"
                            );
                        }
                    }
                } else if let Some(cid) = channel_id {
                    // Fallback: no user_id or no linked Slack id — stored channel
                    for surface in registry.all() {
                        match surface
                            .send_message(&OutboundMessage {
                                channel: cid.clone(),
                                text: text.clone(),
                                thread_id: None,
                                username: None,
                                icon_url: None,

                                blocks: None,
                            })
                            .await
                        {
                            Ok(_) => delivered = true,
                            Err(e) => {
                                error!(
                                    job_id = %job.id,
                                    error = %e,
                                    "Failed to deliver system event to channel"
                                );
                            }
                        }
                    }
                } else {
                    warn!(job_id = %job.id, name = %job.name, "No user_id or channel_id — skipping surface delivery");
                }

                // Also broadcast via WebSocket callback
                if let Some(ref delivery) = self.delivery {
                    delivery(job.name.clone(), text.clone()).await;
                    delivered = true;
                }
                (true, text.clone(), delivered)
            }
            JobPayload::AgentTurn {
                message,
                channel: _,
                channel_id,
                thread_id: _,
                user_id,
                model,
                max_rounds,
                agent_owns_delivery,
                tools,
            } => {
                // Pre-flight budget check so a denied job records a run
                // FAILURE (feeding the consecutive-failure auto-disable)
                // instead of delivering the same refusal to its channel on
                // every fire. `process_message` still backstops this.
                if let crate::cost_governor::Admission::Deny(reason) = service
                    .cost_governor
                    .admit_turn(service.agent_id.as_str(), job.effective_billing_identity())
                {
                    warn!(job_id = %job.id, name = %job.name, %reason, "Job run denied by budget");
                    return self.finish_record(job, started_at, start, false, reason, false);
                }

                // Always deliver to DM via open_conversation for reliability.
                // Resolve DM channel up front so we can pass it to process_message.
                let effective_cid = match user_id
                    .as_ref()
                    .map(|uid| (uid, self.slack_user_for_dm(uid)))
                {
                    Some((_, Some(slack_uid))) => {
                        let registry = service.surface_registry.read().await;
                        match registry
                            .resolve_user_dm(
                                "slack",
                                &slack_uid,
                                workforce_common::channels::ChannelHint::default(),
                            )
                            .await
                        {
                            Ok((_, dm_channel)) => Some(dm_channel),
                            Err(e) => {
                                warn!(
                                    job_id = %job.id,
                                    user_id = %slack_uid,
                                    error = %e,
                                    "Failed to open DM for agent job, falling back to stored channel"
                                );
                                channel_id.clone()
                            }
                        }
                    }
                    Some((uid, None)) => {
                        warn!(
                            job_id = %job.id,
                            user_id = %uid,
                            "Job owner has no linked Slack id — using stored channel"
                        );
                        channel_id.clone()
                    }
                    None => channel_id.clone(),
                };
                let author = run_author(job, &self.jobs.read().await);

                // The task_id mirrors the per-run channel so every
                // cost_record emitted by `process_message` carries it,
                // which the scheduler-history LEFT JOIN then aggregates.
                let started_at = Utc::now();
                let isolated_channel = isolated_run_channel("scheduler", &job.id, started_at);
                let ch = session.as_deref().unwrap_or(&isolated_channel);
                let _alone_in_its_session = match session {
                    Some(_) => Some(self.pinned_run.lock().await),
                    None => None,
                };
                let task_id = isolated_channel.clone();

                // Scope the delivery channel for the run's messaging tools. The
                // prompt's per-user privacy is decided from the `channel_id` and
                // `author` arguments below (see `prompt_privacy`), not this
                // task-local, so a job delivering to a shared channel, or one
                // anybody but its run_as identity is in charge of, never loads
                // that identity's private context.
                let process_result = workforce_agent::runner::CURRENT_CHANNEL_ID
                    .scope(
                        effective_cid.clone().unwrap_or_default(),
                        workforce_agent::runner::with_tool_whitelist(
                            tools.as_deref().map(|tools| match session {
                                Some(_) => workforce_agent::runner::ToolWhitelist::for_turn(tools),
                                None => {
                                    workforce_agent::runner::ToolWhitelist::for_own_session(tools)
                                }
                            }),
                            service.process_turn(
                                ch,
                                "scheduler",
                                message,
                                None,
                                None,
                                effective_cid.as_deref(),
                                None, // Always top-level DM, not thread
                                None,
                                *max_rounds,
                                model.as_deref(),
                                Some(&task_id),
                                None,
                                job.effective_run_as_identity(),
                                author,
                            ),
                        ),
                    )
                    .await;
                let process_result = match process_result {
                    Ok(crate::serve::TurnOutcome::Answered(response)) => Ok((response, true)),
                    Ok(crate::serve::TurnOutcome::Stopped(stop)) => {
                        warn!(job_id = %job.id, name = %job.name, reason = %stop.reason(), "Job run stopped");
                        // A suspension is told nowhere: the job is held.
                        match stop.notice() {
                            Some(notice) => Ok((notice, false)),
                            None => {
                                return self.finish_record(
                                    job,
                                    started_at,
                                    start,
                                    false,
                                    stop.reason(),
                                    false,
                                )
                            }
                        }
                    }
                    Ok(crate::serve::TurnOutcome::Skipped(skip)) => {
                        warn!(
                            job_id = %job.id,
                            name = %job.name,
                            reason = %skip,
                            "Job run skipped"
                        );
                        return self.finish_record(
                            job,
                            started_at,
                            start,
                            false,
                            skip.to_string(),
                            false,
                        );
                    }
                    Err(e) => Err(e),
                };

                let finished_at = Utc::now();
                let duration_ms = (finished_at - started_at).num_milliseconds().max(0) as u64;
                let (success, error_str) = match &process_result {
                    Ok((_, true)) => (true, None),
                    Ok((reason, false)) => (false, Some(reason.clone())),
                    Err(e) => (false, Some(e.to_string())),
                };
                if let Err(e) = service.telemetry_store.record_task_telemetry(
                    &workforce_common::persistence::TaskTelemetryRecord {
                        agent_id: service.agent_id.as_str().to_string(),
                        model: String::new(),
                        provider: "scheduler".to_string(),
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        cost_usd: 0.0,
                        rounds: 0,
                        duration_ms,
                        tool_invocation_count: 0,
                        success,
                        error_type: error_str
                            .as_deref()
                            .map(|s| workforce_common::text::truncate_with_ellipsis(s, 80)),
                        retry_count: 0,
                        task_type: Some(format!("scheduler:{}", job.id)),
                        task_id: Some(task_id.clone()),
                        started_at: started_at.to_rfc3339(),
                        completed_at: finished_at.to_rfc3339(),
                        forecast_cost_usd: None,
                        forecast_rounds: None,
                        forecast_confidence: None,
                        team_id: None,
                        session_id: None,
                        model_switches: None,
                        ticket_id: None,
                        identity_id: job.effective_billing_identity().map(str::to_string),
                        description: Some(workforce_common::text::truncate_with_ellipsis(
                            &job.name, 200,
                        )),
                    },
                    &[],
                    &[],
                ) {
                    warn!(job_id = %job.id, error = %e, "Failed to record scheduler telemetry");
                }

                match process_result {
                    Ok((response, answered)) => {
                        info!(job_id = %job.id, name = %job.name, "Agent job executed");
                        // A stopped run's reason is the scheduler's to deliver
                        // whoever owns delivery: the agent posted no result.
                        let agent_owns_delivery = *agent_owns_delivery && answered;
                        // A job asked from a voice bridge answers on it; the
                        // Slack DM below stays the fallback. Self-delivering
                        // jobs already posted during the turn, so an
                        // announcement would double-deliver.
                        let mut delivered = if agent_owns_delivery {
                            false
                        } else {
                            self.deliver_via_voice(job, &response).await
                        };

                        // Send the agent's final text to the job's destination —
                        // UNLESS the job owns its own delivery (it self-posts the
                        // result during the turn), in which case the scheduler
                        // staying silent is what prevents the duplicate
                        // "report + chatty `Done`" double-post.
                        if !delivered {
                            if agent_owns_delivery || effective_cid.is_none() {
                                // Self-posting jobs and jobs with no destination
                                // have nothing for the scheduler to deliver; an
                                // undelivered run must mean a real send failure so
                                // the tick's failure counter stays meaningful.
                                delivered = true;
                            } else if let Some(ref cid) = effective_cid {
                                let registry = service.surface_registry.read().await;
                                for surface in registry.all() {
                                    match surface
                                        .send_message(&OutboundMessage {
                                            channel: cid.clone(),
                                            text: response.clone(),
                                            thread_id: None,
                                            username: None,
                                            icon_url: None,

                                            blocks: None,
                                        })
                                        .await
                                    {
                                        Ok(_) => delivered = true,
                                        Err(e) => {
                                            error!(
                                                job_id = %job.id,
                                                platform = %surface.platform(),
                                                error = %e,
                                                "Failed to deliver agent response to surface"
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        // Also broadcast via WebSocket callback
                        if let Some(ref delivery) = self.delivery {
                            delivery(job.name.clone(), response.clone()).await;
                            delivered = true;
                        }
                        (answered, response, delivered)
                    }
                    Err(e) => {
                        error!(job_id = %job.id, error = %e, "Agent job execution failed");
                        (false, e.to_string(), false)
                    }
                }
            }
            JobPayload::Reminder { text, user_id } => {
                info!(job_id = %job.id, name = %job.name, user_id = %user_id, "Delivering reminder");
                // The device the reminder was asked from comes first; a voice
                // failure (bridge offline) falls back to the Slack DM, and a
                // failure of both rides the existing retry-with-backoff, so
                // the reminder is announced when the bridge returns.
                let mut delivered = self.deliver_via_voice(job, text).await;

                if !delivered {
                    let registry = service.surface_registry.read().await;

                    let dm_result = match self.slack_user_for_dm(user_id) {
                        Some(slack_uid) => {
                            registry
                                .resolve_user_dm(
                                    "slack",
                                    &slack_uid,
                                    workforce_common::channels::ChannelHint::default(),
                                )
                                .await
                        }
                        None => Err(eyre::eyre!(
                            "reminder owner '{user_id}' has no linked Slack id"
                        )),
                    };
                    match dm_result {
                        Ok((surface, dm_channel)) => {
                            let reminder_text = format!("\u{1f514} *Reminder:* {}", text);
                            match surface
                                .send_message(&OutboundMessage {
                                    channel: dm_channel,
                                    text: reminder_text,
                                    thread_id: None,
                                    username: None,
                                    icon_url: None,
                                    blocks: None,
                                })
                                .await
                            {
                                Ok(_) => {
                                    delivered = true;
                                    info!(
                                        job_id = %job.id,
                                        user_id = %user_id,
                                        "Reminder delivered via DM"
                                    );
                                }
                                Err(e) => {
                                    error!(
                                        job_id = %job.id,
                                        error = %e,
                                        "Failed to deliver reminder"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            error!(
                                job_id = %job.id,
                                user_id = %user_id,
                                error = %e,
                                "Failed to open DM for reminder"
                            );
                        }
                    }
                }

                (true, text.clone(), delivered)
            }
            JobPayload::WorkflowStart {
                workflow_id,
                payload,
                channel_id,
                thread_id,
            } => match service.workflow_runtime() {
                None => {
                    error!(
                        job_id = %job.id,
                        workflow_id = %workflow_id,
                        "WorkflowStart dispatched but WorkflowRuntime is not registered on AgentService"
                    );
                    (false, "WorkflowRuntime not available".to_string(), false)
                }
                Some(runtime) => match runtime
                    .automated_run_block(workflow_id, job.effective_run_as_identity())
                    .await
                {
                    // A suspended person's schedule is held, not failing: it
                    // must not count towards the auto-disable that would
                    // leave it off once they are back. The next schedule
                    // sync drops it until then.
                    Some(crate::workflow_runtime::AutomatedRunBlock::Inactive)
                        if job
                            .effective_run_as_identity()
                            .is_some_and(|person| service.suspended(person).is_some()) =>
                    {
                        info!(
                            job_id = %job.id,
                            workflow_id = %workflow_id,
                            "Scheduled workflow held: the person it runs as is suspended"
                        );
                        (
                            true,
                            format!("{SKIPPED_PREFIX} the person it runs as is suspended"),
                            false,
                        )
                    }
                    Some(block) => {
                        warn!(
                            job_id = %job.id,
                            workflow_id = %workflow_id,
                            %block,
                            "Scheduled workflow suspended"
                        );
                        (false, format!("suspended: {block}"), false)
                    }
                    // A slot firing while the workflow already has `max_concurrent`
                    // instances active (running, or parked at an approval gate)
                    // would stack another and its duplicate channel posts. Skip
                    // counts as a successful occurrence — the job advances to its
                    // next slot — and names a blocking instance so a starved
                    // schedule is diagnosable from the run history. A store error
                    // falls through to start: the guard must not wedge a schedule.
                    None => {
                        let active =
                            runtime
                                .blocking_instance(workflow_id)
                                .await
                                .unwrap_or_else(|e| {
                                    warn!(
                                        job_id = %job.id,
                                        workflow_id = %workflow_id,
                                        error = %e,
                                        "Concurrency check failed — starting anyway"
                                    );
                                    None
                                });
                        if let Some(active) = active {
                            warn!(
                                job_id = %job.id,
                                workflow_id = %workflow_id,
                                blocking_instance = %active,
                                "Skipping scheduled workflow start: the workflow is at its max_concurrent"
                            );
                            (
                                true,
                                format!("{SKIPPED_PREFIX} instance {active} still active and the workflow is at max_concurrent"),
                                false,
                            )
                        } else {
                            match Arc::clone(runtime)
                                .start_workflow(
                                    workflow_id,
                                    payload.clone(),
                                    channel_id.clone(),
                                    thread_id.clone(),
                                    None,
                                    None,
                                )
                                .await
                            {
                                Ok(instance_id) => {
                                    info!(
                                        job_id = %job.id,
                                        workflow_id = %workflow_id,
                                        instance_id = %instance_id,
                                        "Scheduled workflow started"
                                    );
                                    (true, format!("started instance {instance_id}"), false)
                                }
                                Err(e) => {
                                    error!(
                                        job_id = %job.id,
                                        workflow_id = %workflow_id,
                                        error = %e,
                                        "Failed to start scheduled workflow"
                                    );
                                    (false, e.to_string(), false)
                                }
                            }
                        }
                    }
                },
            },
            JobPayload::MonitorSweep => match service.monitor_engine() {
                None => (false, "MonitorEngine not available".to_string(), false),
                Some(engine) => {
                    engine.sweep_all().await;
                    (true, "monitor sweep complete".to_string(), false)
                }
            },
            JobPayload::Consolidation { identity_id } => {
                match crate::memory_consolidation::execute(
                    &service,
                    identity_id.as_deref(),
                    &job.id,
                    self.cancel.get(),
                )
                .await
                {
                    Ok(summary) => (true, summary, true),
                    Err(e) => (false, e.to_string(), false),
                }
            }
        };

        JobRunRecord {
            note: note.map(str::to_string),
            ..self.finish_record(job, started_at, start, success, output, delivered)
        }
    }

    fn finish_record(
        &self,
        job: &ScheduledJob,
        started_at: DateTime<Utc>,
        start: std::time::Instant,
        success: bool,
        output: String,
        delivered: bool,
    ) -> JobRunRecord {
        JobRunRecord {
            job_id: job.id.clone(),
            job_name: job.name.clone(),
            started_at,
            finished_at: Utc::now(),
            duration_ms: start.elapsed().as_millis() as u64,
            success,
            output: workforce_common::text::truncate_str(&output, 500).to_string(),
            delivered,
            note: None,
        }
    }

    /// Push a run record into history and persist.
    async fn push_history(&self, record: JobRunRecord) {
        let mut history = self.run_history.write().await;
        self.persist_run_record(&record);
        history.push(record);
        if history.len() > self.max_history {
            let excess = history.len() - self.max_history;
            history.drain(..excess);
        }
    }

    /// Persist a run record to JSONL file.
    fn persist_run_record(&self, record: &JobRunRecord) {
        let dir = self.data_dir.join("scheduler");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let path = dir.join("run-history.jsonl");
        if let Ok(line) = serde_json::to_string(record) {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "{}", line);
            }
        }
    }

    /// The persistence backend, built from `[scheduler_store]` on first use.
    async fn job_store(&self) -> eyre::Result<Arc<dyn crate::scheduler_store::JobStore>> {
        self.store
            .get_or_try_init(|| async {
                let service = self.service();
                crate::scheduler_store::build_job_store(
                    &service.config,
                    &self.data_dir,
                    service.agent_id.as_str(),
                )
                .await
            })
            .await
            .cloned()
    }

    /// Persist a mutation. The file backend rewrites the whole list; the
    /// Postgres backend applies exactly `change`, so peer instances can't
    /// clobber each other's jobs. Failures warn (as the file write always
    /// has). MUST be called while still holding the `jobs` write guard that
    /// made the mutation (pass its contents as `all`): the memory write and
    /// the store write form one atomic step against
    /// [`Self::refresh_from_shared_store`], which takes the same lock —
    /// otherwise a refresh could resurrect a just-deleted job.
    async fn persist_change(
        &self,
        all: &[ScheduledJob],
        change: crate::scheduler_store::JobChange<'_>,
    ) {
        let store = match self.job_store().await {
            Ok(store) => store,
            Err(e) => {
                warn!(error = %e, "Scheduler job store unavailable — jobs not persisted");
                return;
            }
        };
        if let Err(e) = store.persist(all, change).await {
            warn!(error = %e, "Failed to persist scheduler jobs");
        }
    }

    /// On a shared (Postgres) backend, refresh the in-memory list so edits
    /// made by peer instances become visible — at most every 5s. The write
    /// lock is held across the load AND the swap so no local mutation (which
    /// persists under the same lock) can interleave and be reverted.
    async fn refresh_from_shared_store(&self) {
        let due = refresh_due(
            &mut self
                .last_store_refresh
                .lock()
                .expect("refresh mutex poisoned"),
            std::time::Duration::from_secs(5),
        );
        if due {
            let mut jobs = self.jobs.write().await;
            self.reload_from_shared_store(&mut jobs).await;
        }
    }

    /// Replace `jobs` (the list under its write guard) with what a shared
    /// store holds now. A store only this instance writes is already what
    /// `jobs` holds.
    async fn reload_from_shared_store(&self, jobs: &mut Vec<ScheduledJob>) {
        let Ok(store) = self.job_store().await else {
            return;
        };
        if !store.is_shared() {
            return;
        }
        match store.load_all().await {
            Ok(fresh) => *jobs = fresh,
            Err(e) => warn!(error = %e, "Refreshing scheduler jobs from shared store failed"),
        }
    }
}

/// Whether a shared-store refresh is due, stamping `last` when it is.
fn refresh_due(last: &mut Option<std::time::Instant>, interval: std::time::Duration) -> bool {
    match *last {
        Some(at) if at.elapsed() < interval => false,
        _ => {
            *last = Some(std::time::Instant::now());
            true
        }
    }
}

/// Calculate the next run time for a schedule.
///
/// `default_tz` is used for cron expressions that don't specify their own timezone.
/// Reject schedules an owned (human/agent-created) job may not use: anything
/// recurring faster than `min_secs`, a zero interval, or an unparseable cron.
/// `min_secs == 0` disables the floor. One-shots (`At`) always pass.
pub fn validate_schedule(
    schedule: &Schedule,
    min_secs: u64,
    now: &DateTime<Utc>,
    default_tz: Tz,
) -> Result<(), String> {
    match schedule {
        Schedule::At { .. } => Ok(()),
        Schedule::Every { every_secs, .. } => {
            if *every_secs == 0 {
                return Err("`every_secs` must be greater than zero".to_string());
            }
            if min_secs > 0 && *every_secs < min_secs {
                return Err(format!(
                    "Recurring interval {every_secs}s is below the minimum of {min_secs}s \
                     (`[budget] min_recurring_interval_secs`). Use a coarser schedule, or an \
                     event-driven monitor for reply/urgent-change watching."
                ));
            }
            Ok(())
        }
        Schedule::Cron { expr, tz } => {
            let first = cron_next_run(expr, tz.as_deref(), now, default_tz)
                .ok_or_else(|| format!("Invalid cron expression '{expr}'"))?;
            if min_secs > 0 {
                let probe = first + chrono::Duration::seconds(1);
                if let Some(second) = cron_next_run(expr, tz.as_deref(), &probe, default_tz) {
                    let gap = (second - first).num_seconds().max(0) as u64;
                    if gap < min_secs {
                        return Err(format!(
                            "Cron '{expr}' fires every {gap}s, below the minimum of {min_secs}s \
                             (`[budget] min_recurring_interval_secs`)."
                        ));
                    }
                }
            }
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl crate::hosts::SchedulerHost for Scheduler {
    async fn set_messaging_context(
        &self,
        session_key: &str,
        channel_id: String,
        thread_id: Option<String>,
    ) {
        Scheduler::set_messaging_context(self, session_key, channel_id, thread_id).await
    }

    async fn get_messaging_context(&self, session_key: &str) -> Option<(String, Option<String>)> {
        Scheduler::get_messaging_context(self, session_key).await
    }

    async fn clear_messaging_context(&self, session_key: &str) {
        Scheduler::clear_messaging_context(self, session_key).await
    }

    async fn list_jobs(&self) -> Vec<ScheduledJob> {
        Scheduler::list_jobs(self).await
    }

    async fn get_job(&self, job_id: &str) -> Option<ScheduledJob> {
        Scheduler::get_job(self, job_id).await
    }

    fn job_spend(&self, job_id: &str) -> Option<f64> {
        Scheduler::job_spend(self, job_id)
    }

    async fn add_job_with_desc(
        &self,
        name: &str,
        description: Option<&str>,
        schedule: Schedule,
        payload: JobPayload,
    ) -> Result<String, String> {
        Scheduler::add_job_with_desc(self, name, description, schedule, payload).await
    }

    async fn add_job_owned(
        &self,
        name: &str,
        description: Option<&str>,
        schedule: Schedule,
        payload: JobPayload,
        creator_identity_id: Option<String>,
        visibility: Option<String>,
    ) -> Result<String, String> {
        Scheduler::add_job_owned(
            self,
            name,
            description,
            schedule,
            payload,
            creator_identity_id,
            visibility,
        )
        .await
    }

    async fn update_job(
        &self,
        job_id: &str,
        edit: JobEdit,
        editor: &str,
    ) -> Result<Option<ScheduledJob>, EditRefused> {
        Scheduler::update_job(self, job_id, edit, editor).await
    }

    async fn remove_job(&self, job_id: &str) -> bool {
        Scheduler::remove_job(self, job_id).await
    }

    async fn remove_workflow_start_jobs(&self) -> usize {
        Scheduler::remove_workflow_start_jobs(self).await
    }

    async fn run_job_now(&self, job_id: &str) -> Option<JobRunRecord> {
        Scheduler::run_job_now(self, job_id).await
    }

    async fn recent_outcomes(&self, job_id: &str, n: usize) -> Outcomes {
        Scheduler::recent_outcomes(self, job_id, n).await
    }

    async fn current_jobs(&self) -> Vec<ScheduledJob> {
        Scheduler::current_jobs(self).await
    }

    async fn forget_jobs(&self, ids: &[String]) -> usize {
        Scheduler::forget_jobs(self, ids).await
    }

    async fn release_jobs(&self, names: &[String]) -> Vec<String> {
        Scheduler::release_jobs(self, names).await
    }

    async fn hold_for_suspension(&self, names: &[String], apply: bool) -> Vec<ScheduledJob> {
        Scheduler::hold_for_suspension(self, names, apply).await
    }

    async fn reassign_jobs(
        &self,
        ids: Option<&[String]>,
        names: &[String],
        to: &str,
        editor: Option<&str>,
    ) -> Vec<String> {
        Scheduler::reassign_jobs(self, ids, names, to, editor).await
    }

    async fn release_suspension_holds(
        &self,
        suspended: &(dyn for<'p> Fn(&'p str) -> bool + Send + Sync),
        apply: bool,
    ) -> Vec<ScheduledJob> {
        Scheduler::release_suspension_holds(self, suspended, apply).await
    }

    async fn jobs_naming(&self, names: &[String]) -> Vec<ScheduledJob> {
        Scheduler::jobs_naming(self, names).await
    }

    async fn upsert_job(
        &self,
        name: &str,
        description: Option<&str>,
        schedule: Schedule,
        payload: JobPayload,
    ) -> Result<String, String> {
        Scheduler::upsert_job(self, name, description, schedule, payload).await
    }

    async fn update_job_run_as(&self, job_id: &str, identity_id: Option<String>) -> bool {
        Scheduler::update_job_run_as(self, job_id, identity_id).await
    }

    async fn update_job_identity(&self, job_id: &str, identity_id: Option<String>) -> bool {
        Scheduler::update_job_identity(self, job_id, identity_id).await
    }

    async fn update_job_bill_to(&self, job_id: &str, identity_id: Option<String>) -> bool {
        Scheduler::update_job_bill_to(self, job_id, identity_id).await
    }

    async fn job_run_history(&self, job_id: &str) -> Vec<JobRunRecord> {
        Scheduler::job_run_history(self, job_id).await
    }

    fn retire(&self) {
        Scheduler::retire(self)
    }

    async fn run(self: Arc<Self>, cancel: tokio_util::sync::CancellationToken) {
        Scheduler::run(self, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub_core::schedules::{
        continued_session_channel, pinned_session_channel, PINNED_SESSION_PREFIX,
    };
    use crate::serve::Compose as _;
    use chrono::TimeZone;

    const DEFAULT_TZ: Tz = chrono_tz::UTC;

    fn job_run(output: &str, success: bool) -> JobRunRecord {
        JobRunRecord {
            job_id: "job".into(),
            job_name: "job".into(),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            duration_ms: 0,
            success,
            output: output.into(),
            delivered: false,
            note: None,
        }
    }

    #[test]
    fn outcomes_classify_the_latest_runs_as_skipped_at_the_cap_failed_or_started() {
        let runs = [
            job_run("done", true),
            job_run("boom", false),
            job_run(
                &format!("{SKIPPED_PREFIX} instance a still active and the workflow is at max_concurrent"),
                true,
            ),
            job_run("started", true),
            job_run("suspended: the owner is inactive", false),
        ];
        assert_eq!(
            outcomes(&runs, 12),
            Outcomes {
                considered: 5,
                started: 2,
                skipped: 1,
                failed: 2
            }
        );
        assert_eq!(
            outcomes(&runs, 3),
            Outcomes {
                considered: 3,
                started: 1,
                skipped: 1,
                failed: 1
            }
        );
        assert_eq!(outcomes(&runs[..0], 12), Outcomes::default());
    }

    #[test]
    fn refresh_due_first_call_then_gated_then_due_again() {
        let mut last = None;
        let gate = std::time::Duration::from_millis(50);
        assert!(refresh_due(&mut last, gate), "first call always refreshes");
        assert!(!refresh_due(&mut last, gate), "inside the window: gated");
        std::thread::sleep(gate);
        assert!(refresh_due(&mut last, gate), "past the window: due again");
    }

    #[test]
    fn next_run_at() {
        let now = Utc::now();
        let future = now + chrono::Duration::hours(1);
        let past = now - chrono::Duration::hours(1);

        assert!(next_run_time(&Schedule::At { at: future }, &now, DEFAULT_TZ).is_some());
        assert!(next_run_time(&Schedule::At { at: past }, &now, DEFAULT_TZ).is_none());
    }

    #[test]
    fn validate_schedule_floors_every() {
        let now = Utc::now();
        let every = |secs| Schedule::Every {
            every_secs: secs,
            anchor: None,
        };
        assert!(validate_schedule(&every(0), 300, &now, DEFAULT_TZ).is_err());
        assert!(validate_schedule(&every(13), 300, &now, DEFAULT_TZ).is_err());
        assert!(validate_schedule(&every(299), 300, &now, DEFAULT_TZ).is_err());
        assert!(validate_schedule(&every(300), 300, &now, DEFAULT_TZ).is_ok());
        assert!(validate_schedule(&every(3600), 300, &now, DEFAULT_TZ).is_ok());
        // Floor disabled: only the zero interval is rejected.
        assert!(validate_schedule(&every(13), 0, &now, DEFAULT_TZ).is_ok());
        assert!(validate_schedule(&every(0), 0, &now, DEFAULT_TZ).is_err());
    }

    #[test]
    fn validate_schedule_floors_cron_and_rejects_garbage() {
        let now = Utc::now();
        let cron = |expr: &str| Schedule::Cron {
            expr: expr.to_string(),
            tz: None,
        };
        // Every-minute cron is below a 300s floor; hourly and daily pass.
        assert!(validate_schedule(&cron("* * * * *"), 300, &now, DEFAULT_TZ).is_err());
        assert!(validate_schedule(&cron("0 * * * *"), 300, &now, DEFAULT_TZ).is_ok());
        assert!(validate_schedule(&cron("0 9 * * 1"), 300, &now, DEFAULT_TZ).is_ok());
        // Unparseable cron is rejected even with the floor off (previously it
        // was silently accepted and never fired).
        assert!(validate_schedule(&cron("not a cron"), 0, &now, DEFAULT_TZ).is_err());
    }

    #[test]
    fn validate_schedule_never_rejects_one_shots() {
        let now = Utc::now();
        let at = Schedule::At {
            at: now + chrono::Duration::seconds(5),
        };
        assert!(validate_schedule(&at, 300, &now, DEFAULT_TZ).is_ok());
    }

    #[test]
    fn runs_per_day_projects_cadence() {
        let now = Utc::now();
        let every = |secs| Schedule::Every {
            every_secs: secs,
            anchor: None,
        };
        assert_eq!(runs_per_day(&every(300), &now, DEFAULT_TZ), 288.0);
        assert_eq!(runs_per_day(&every(86_400), &now, DEFAULT_TZ), 1.0);
        assert_eq!(runs_per_day(&every(0), &now, DEFAULT_TZ), 0.0);
        let hourly = Schedule::Cron {
            expr: "0 * * * *".to_string(),
            tz: None,
        };
        assert_eq!(runs_per_day(&hourly, &now, DEFAULT_TZ), 24.0);
        let at = Schedule::At { at: now };
        assert_eq!(runs_per_day(&at, &now, DEFAULT_TZ), 0.0);
    }

    /// A pre-upgrade jobs.json row (no caps fields) must parse — `load_jobs`
    /// drops the WHOLE persisted file on any parse failure.
    #[test]
    fn legacy_job_row_without_cap_fields_parses() {
        let legacy = serde_json::json!({
            "id": "legacy-1",
            "name": "legacy",
            "schedule": { "kind": "every", "every_secs": 300 },
            "payload": { "kind": "agent_turn", "message": "hi" },
            "enabled": true,
            "created_at": "2026-08-13T15:00:18Z",
            "last_run": null,
            "next_run": null,
            "run_count": 42
        });
        let job: ScheduledJob = serde_json::from_value(legacy).unwrap();
        assert_eq!(job.run_count, 42);
        assert!(job.max_runs.is_none());
        assert!(job.budget_usd.is_none());
        assert!(job.expires_at.is_none());
    }

    fn test_config() -> workforce_common::settings::WorkforceConfig {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = workforce_common::settings::WorkforceConfig::default();
        cfg.hub.data_dir = tmp.path().to_string_lossy().to_string();
        cfg.memory.db_path = std::path::Path::new(&cfg.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        cfg.llm.api_key = Some("test-key".into());
        std::mem::forget(tmp);
        cfg
    }

    async fn test_service() -> Arc<AgentService> {
        crate::serve::AgentService::from_config(test_config(), None)
            .await
            .expect("test service")
    }

    async fn test_scheduler() -> Arc<Scheduler> {
        Arc::new(Scheduler::new(test_service().await))
    }

    fn noop_delivery() -> DeliveryCallback {
        Arc::new(|_: String, _: String| Box::pin(async {}) as _)
    }

    /// The creator a reminder added inside a turn run as `principal`, and
    /// billed to `billed`, records.
    async fn creator_in_turn(sched: &Scheduler, principal: &str, billed: &str) -> Option<String> {
        let id = workforce_agent::runner::CURRENT_USER_ID
            .scope(
                principal.to_string(),
                workforce_agent::runner::CURRENT_BILLING_IDENTITY.scope(
                    billed.to_string(),
                    sched.add_job(
                        "check in",
                        Schedule::Every {
                            every_secs: 86_400,
                            anchor: None,
                        },
                        JobPayload::Reminder {
                            text: "check in".into(),
                            user_id: "U1".into(),
                        },
                    ),
                ),
            )
            .await
            .unwrap();
        sched.get_job(&id).await.unwrap().creator_identity_id
    }

    #[tokio::test]
    async fn job_created_in_sentinel_turn_is_agent_owned() {
        const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
        let sched = test_scheduler().await;
        let agent = format!("agent:{}", sched.service().agent_id);
        assert_eq!(
            creator_in_turn(&sched, "heartbeat", "").await,
            Some(agent.clone()),
            "a heartbeat's job runs as the agent, not as the sentinel"
        );
        assert_eq!(
            creator_in_turn(&sched, "system:consolidation", "")
                .await
                .as_deref(),
            Some("system:consolidation")
        );
        assert_eq!(
            creator_in_turn(&sched, ALICE, "").await.as_deref(),
            Some(ALICE)
        );
        assert_eq!(
            creator_in_turn(&sched, "system", "").await.as_deref(),
            Some("system"),
            "the install's own work is not promoted to the agent"
        );
        assert_eq!(creator_in_turn(&sched, "", "").await, None);
        assert_eq!(
            creator_in_turn(&sched, "heartbeat", ALICE).await,
            Some(agent),
            "a heartbeat billed to its owner never creates jobs as the owner"
        );
    }

    #[tokio::test]
    async fn background_jobs_bill_the_owner_install_work_the_operator() {
        const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
        const BOB: &str = "0b7f3d52-8f0a-4c2f-9d44-3c1e6a1f4e22";
        let mut config = test_config();
        config.subscription.operator = Some(uuid::Uuid::parse_str(BOB).unwrap());
        config.agents.push(workforce_common::settings::AgentConfig {
            id: "allan".into(),
            default: true,
            owner_id: Some(ALICE.into()),
            ..Default::default()
        });
        let service = crate::serve::AgentService::from_config(config, None)
            .await
            .expect("test service");
        let sched = Scheduler::new(service);
        let job = |creator: Option<&str>, bill_to: Option<&str>| ScheduledJob {
            bill_to_identity_id: bill_to.map(str::to_string),
            ..job_fixture(creator, None, agent_turn())
        };

        assert_eq!(sched.billing_of(&job(None, None)).as_deref(), Some(ALICE));
        assert_eq!(
            sched.billing_of(&job(Some("agent:allan"), None)).as_deref(),
            Some(ALICE),
            "the agent's own jobs bill its owner"
        );
        assert_eq!(
            sched.billing_of(&job(Some("system"), None)).as_deref(),
            Some(BOB),
            "the install's work bills its operator"
        );
        assert_eq!(
            sched
                .billing_of(&job(Some("system:consolidation"), None))
                .as_deref(),
            Some(BOB),
            "budget-exempt install work is paid for like any other"
        );
        assert_eq!(
            sched
                .billing_of(&job(Some("heartbeat"), Some("system:indexing")))
                .as_deref(),
            Some("system:indexing"),
            "an explicit bill-to wins"
        );
        assert_eq!(
            sched.billing_of(&job(Some(BOB), None)),
            None,
            "a person's job is theirs to pay for"
        );
    }

    #[tokio::test]
    async fn job_created_in_games_turn_is_never_promoted_to_the_agent() {
        let sched = test_scheduler().await;
        assert_eq!(
            creator_in_turn(&sched, "games", "").await.as_deref(),
            Some("games"),
            "an untrusted game turn is never promoted to the agent's own plan"
        );
    }

    #[tokio::test]
    async fn jobs_run_against_the_seed_until_an_owner_adopts_the_scheduler() {
        let seed = test_service().await;
        let sched = Scheduler::new(Arc::clone(&seed));
        assert!(Arc::ptr_eq(&sched.service(), &seed));

        let owner = test_service().await;
        let strong = Arc::strong_count(&owner);
        sched.set_owner(Arc::downgrade(&owner));
        assert!(Arc::ptr_eq(&sched.service(), &owner));
        assert_eq!(
            Arc::strong_count(&owner),
            strong,
            "adoption must not hold the owner strongly"
        );

        sched.set_owner(Arc::downgrade(&seed));
        assert!(
            Arc::ptr_eq(&sched.service(), &owner),
            "adoption is once-only"
        );
    }

    #[tokio::test]
    async fn the_default_scheduler_runs_jobs_against_the_service_that_owns_it() {
        let config = test_config();
        let subscriptions = crate::subscription::Subscriptions::boot(&config, None);
        let (service, scheduler) =
            crate::serve::AgentService::from_config_with_scheduler_delivery_and_reactions(
                config,
                None,
                noop_delivery(),
                None,
                false,
                subscriptions.clone(),
            )
            .await
            .expect("test service");
        assert!(service
            .scheduler
            .as_ref()
            .is_some_and(|s| std::ptr::addr_eq(Arc::as_ptr(s), Arc::as_ptr(&scheduler))));
        assert!(Arc::ptr_eq(&scheduler.service(), &service));
        let runtime = scheduler
            .service()
            .workflow_runtime()
            .cloned()
            .expect("owner exposes a runtime");
        assert!(Arc::ptr_eq(
            &runtime,
            service.workflow_runtime().expect("owner exposes a runtime")
        ));
        assert!(runtime.can_schedule());
    }

    #[tokio::test]
    async fn an_additional_agent_scheduler_runs_jobs_against_the_service_that_owns_it() {
        let shared = test_service().await;
        let agent = workforce_common::settings::AgentConfig {
            id: "second".into(),
            ..Default::default()
        };
        let config = test_config();
        let subscriptions = crate::subscription::Subscriptions::boot(&config, None);
        let (service, scheduler) = crate::serve::AgentService::build_for_agent_with_scheduler(
            config,
            None,
            &agent,
            noop_delivery(),
            Arc::new(workforce_common::locking::LockManager::new()),
            None,
            None,
            shared.workflow_runtime().cloned(),
            None,
            subscriptions,
        )
        .await
        .expect("agent service");
        assert!(service
            .scheduler
            .as_ref()
            .is_some_and(|s| std::ptr::addr_eq(Arc::as_ptr(s), Arc::as_ptr(&scheduler))));
        assert!(Arc::ptr_eq(&scheduler.service(), &service));
    }

    async fn fire_due_sweep(sched: &Scheduler) -> usize {
        sched
            .add_job(
                "sweep",
                Schedule::Cron {
                    expr: "* * * * *".to_string(),
                    tz: None,
                },
                JobPayload::MonitorSweep,
            )
            .await
            .unwrap();
        for job in sched.jobs.write().await.iter_mut() {
            job.next_run = Some(Utc::now() - chrono::Duration::seconds(1));
        }
        sched.tick().await;
        sched.run_history().await.len()
    }

    #[tokio::test]
    async fn a_retired_scheduler_stops_firing() {
        let live = test_scheduler().await;
        live.set_ready();
        assert_eq!(fire_due_sweep(&live).await, 1);

        let retired = test_scheduler().await;
        retired.set_ready();
        retired.retire();
        assert_eq!(fire_due_sweep(&retired).await, 0);
    }

    /// A scheduler whose agent knows an active and a suspended member, with
    /// an hourly reminder owned by each.
    async fn members_with_reminders(
        dir: &std::path::Path,
    ) -> (Arc<Scheduler>, Arc<AgentService>, String, String) {
        let (service, active, suspended) = crate::serve::testing::service_with_members(dir).await;
        let sched = Arc::new(Scheduler::new(Arc::clone(&service)));
        let mut ids = Vec::new();
        for person in [&active, &suspended] {
            ids.push(
                sched
                    .add_job_owned(
                        &format!("stretch-{}", person.display_name),
                        None,
                        hourly(),
                        JobPayload::Reminder {
                            text: "stretch".into(),
                            user_id: person.id.to_string(),
                        },
                        Some(person.id.to_string()),
                        None,
                    )
                    .await
                    .unwrap(),
            );
        }
        let suspended_job = ids.pop().unwrap();
        (sched, service, ids.pop().unwrap(), suspended_job)
    }

    fn suspended_names(service: &AgentService) -> impl Fn(&str) -> bool + '_ {
        |person| service.suspended(person).is_some()
    }

    #[tokio::test]
    async fn a_suspended_persons_due_job_is_held_instead_of_run_and_no_failure_is_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let (sched, _, active_job, held_job) = members_with_reminders(tmp.path()).await;
        sched.set_ready();
        for job in sched.jobs.write().await.iter_mut() {
            job.next_run = Some(Utc::now() - chrono::Duration::seconds(1));
        }
        sched.tick().await;

        let ran: Vec<String> = sched
            .run_history()
            .await
            .into_iter()
            .map(|run| run.job_id)
            .collect();
        assert_eq!(ran, [active_job], "only the active person's job ran");
        let held = sched.get_job(&held_job).await.unwrap();
        assert!(!held.enabled && held.paused_by_suspension);
        assert_eq!(held.delivery_failures, 0);
        assert_eq!(held.run_count, 0);
    }

    #[tokio::test]
    async fn reactivation_releases_exactly_what_the_suspension_held_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (sched, service, _, held_job) = members_with_reminders(tmp.path()).await;
        let suspended = sched.get_job(&held_job).await.unwrap().creator_identity_id;
        let names = vec![suspended.clone().unwrap()];
        let paused_by_them = sched
            .add_job_owned(
                "paused-by-them",
                None,
                hourly(),
                turn_payload(),
                suspended,
                None,
            )
            .await
            .unwrap();
        sched
            .update_job(
                &paused_by_them,
                JobEdit {
                    enabled: Some(false),
                    ..JobEdit::default()
                },
                "them",
            )
            .await
            .unwrap();

        let held: Vec<String> = sched
            .hold_for_suspension(&names, true)
            .await
            .into_iter()
            .map(|job| job.id)
            .collect();
        assert_eq!(
            held,
            std::slice::from_ref(&held_job),
            "a job they paused is not held"
        );
        assert!(sched.hold_for_suspension(&names, true).await.is_empty());
        assert!(
            sched
                .release_suspension_holds(&suspended_names(&service), true)
                .await
                .is_empty(),
            "nothing is released while they are suspended"
        );

        let released: Vec<String> = sched
            .release_suspension_holds(&|_| false, true)
            .await
            .into_iter()
            .map(|job| job.id)
            .collect();
        assert_eq!(released, std::slice::from_ref(&held_job));
        let job = sched.get_job(&held_job).await.unwrap();
        assert!(job.enabled && !job.paused_by_suspension);
        assert!(job.next_run.is_some_and(|next| next > Utc::now()));
        assert!(!sched.get_job(&paused_by_them).await.unwrap().enabled);
        assert!(sched
            .release_suspension_holds(&|_| false, true)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn a_dry_run_hold_or_release_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (sched, _, _, held_job) = members_with_reminders(tmp.path()).await;
        let names = vec![sched
            .get_job(&held_job)
            .await
            .unwrap()
            .creator_identity_id
            .unwrap()];
        assert_eq!(sched.hold_for_suspension(&names, false).await.len(), 1);
        assert!(sched.get_job(&held_job).await.unwrap().enabled);
        sched.hold_for_suspension(&names, true).await;
        assert_eq!(
            sched
                .release_suspension_holds(&|_| false, false)
                .await
                .len(),
            1
        );
        assert!(sched.get_job(&held_job).await.unwrap().paused_by_suspension);
    }

    #[tokio::test]
    async fn switching_a_held_job_on_or_off_by_hand_supersedes_the_hold() {
        let tmp = tempfile::tempdir().unwrap();
        let (sched, _, _, held_job) = members_with_reminders(tmp.path()).await;
        let names = vec![sched
            .get_job(&held_job)
            .await
            .unwrap()
            .creator_identity_id
            .unwrap()];
        sched.hold_for_suspension(&names, true).await;
        sched
            .update_job(
                &held_job,
                JobEdit {
                    enabled: Some(false),
                    ..JobEdit::default()
                },
                "admin",
            )
            .await
            .unwrap();
        assert!(!sched.get_job(&held_job).await.unwrap().paused_by_suspension);
        assert!(sched
            .release_suspension_holds(&|_| false, true)
            .await
            .is_empty());
        assert!(!sched.get_job(&held_job).await.unwrap().enabled);
    }

    #[test]
    fn a_job_stored_before_the_hold_mark_reads_as_unheld() {
        let job = job_fixture(Some("alice"), None, agent_turn());
        let mut stored = serde_json::to_value(&job).unwrap();
        stored
            .as_object_mut()
            .unwrap()
            .remove("paused_by_suspension");
        assert!(
            !serde_json::from_value::<ScheduledJob>(stored)
                .unwrap()
                .paused_by_suspension
        );
    }

    #[test]
    fn only_the_jobs_a_person_runs_are_held_for_them() {
        let job = |creator: Option<&str>, payload| job_fixture(creator, None, payload);
        assert_eq!(job(Some("alice"), agent_turn()).held_for(), Some("alice"));
        assert_eq!(
            job(
                None,
                JobPayload::Reminder {
                    text: "t".into(),
                    user_id: "bob".into()
                }
            )
            .held_for(),
            Some("bob"),
            "a legacy reminder is its recipient's"
        );
        for registered in [
            JobPayload::MonitorSweep,
            JobPayload::Consolidation {
                identity_id: Some("alice".into()),
            },
            JobPayload::WorkflowStart {
                workflow_id: "w".into(),
                payload: serde_json::Value::Null,
                channel_id: None,
                thread_id: None,
            },
        ] {
            assert_eq!(job(Some("alice"), registered).held_for(), None);
        }
    }

    #[tokio::test]
    async fn retire_stops_the_running_loop() {
        let sched = test_scheduler().await;
        let token = tokio_util::sync::CancellationToken::new();
        let running = tokio::spawn(Arc::clone(&sched).run(token.clone()));
        tokio::task::yield_now().await;

        sched.retire();

        assert!(token.is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(1), running)
            .await
            .expect("loop exits once retired")
            .expect("loop task completes");
    }

    #[tokio::test]
    async fn removing_an_agent_from_the_registry_retires_its_scheduler() {
        let config = test_config();
        let subscriptions = crate::subscription::Subscriptions::boot(&config, None);
        let (default_service, _) =
            crate::serve::AgentService::from_config_with_scheduler_delivery_and_reactions(
                config,
                None,
                noop_delivery(),
                None,
                false,
                subscriptions.clone(),
            )
            .await
            .expect("default service");
        let agent = workforce_common::settings::AgentConfig {
            id: "second".into(),
            ..Default::default()
        };
        let (service, scheduler) = crate::serve::AgentService::build_for_agent_with_scheduler(
            test_config(),
            None,
            &agent,
            noop_delivery(),
            Arc::new(workforce_common::locking::LockManager::new()),
            None,
            None,
            default_service.workflow_runtime().cloned(),
            None,
            subscriptions,
        )
        .await
        .expect("agent service");
        let default_id = default_service.agent_id.as_str().to_string();
        let registry = crate::agent_registry::AgentRegistry::from_parts(
            HashMap::from([
                (default_id.clone(), Arc::clone(&default_service)),
                ("second".to_string(), Arc::clone(&service)),
            ]),
            default_id,
        );
        scheduler.set_ready();

        assert!(registry.remove("second").is_some());
        assert_eq!(fire_due_sweep(&scheduler).await, 0);
    }

    /// A job `owner` made to run as themselves, and a reminder for them.
    async fn owned_jobs(sched: &Scheduler, owner: &str) -> (String, String) {
        let job = sched
            .add_job_with_desc("digest", None, hourly(), turn_payload())
            .await
            .unwrap();
        sched.update_job_identity(&job, Some(owner.into())).await;
        sched.update_job_bill_to(&job, Some(owner.into())).await;
        let reminder = sched
            .add_job_with_desc(
                "stretch",
                None,
                hourly(),
                JobPayload::Reminder {
                    text: "stretch".into(),
                    user_id: "U-ALICE".into(),
                },
            )
            .await
            .unwrap();
        (job, reminder)
    }

    fn alice() -> Vec<String> {
        vec![
            "alice".to_string(),
            "slack:U-ALICE".to_string(),
            "U-ALICE".to_string(),
        ]
    }

    #[tokio::test]
    async fn a_merge_moves_every_field_naming_the_person_and_their_edit_stays_theirs() {
        let sched = test_scheduler().await;
        let (job, reminder) = owned_jobs(&sched, "alice").await;
        sched
            .update_job(
                &job,
                JobEdit {
                    schedule: Some(hourly()),
                    confirmed: true,
                    ..Default::default()
                },
                "alice",
            )
            .await
            .unwrap();
        let other = sched
            .add_job_with_desc("unrelated", None, hourly(), turn_payload())
            .await
            .unwrap();
        assert_eq!(sched.jobs_naming(&alice()).await.len(), 2);

        let mut changed = sched.reassign_jobs(None, &alice(), "bob", None).await;
        changed.sort();
        let mut expected = vec![job.clone(), reminder.clone()];
        expected.sort();
        assert_eq!(changed, expected);
        let moved = sched.get_job(&job).await.unwrap();
        assert_eq!(moved.creator_identity_id.as_deref(), Some("bob"));
        assert_eq!(moved.bill_to_identity_id.as_deref(), Some("bob"));
        assert_eq!(moved.edited_by.as_deref(), Some("bob"));
        assert!(
            is_own(&moved),
            "the same person, under the identity they go by now"
        );
        let JobPayload::Reminder { user_id, .. } = sched.get_job(&reminder).await.unwrap().payload
        else {
            panic!("a reminder");
        };
        assert_eq!(user_id, "bob");
        assert!(sched
            .get_job(&other)
            .await
            .unwrap()
            .creator_identity_id
            .is_none());
        assert!(sched.jobs_naming(&alice()).await.is_empty());
        assert!(sched
            .reassign_jobs(None, &alice(), "bob", None)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn a_transferred_job_counts_as_saved_by_the_admin_who_moved_it() {
        let sched = test_scheduler().await;
        let (job, reminder) = owned_jobs(&sched, "alice").await;
        let ids = [job.clone()];
        assert_eq!(
            sched
                .reassign_jobs(Some(&ids), &alice(), "carol", Some("admin-1"))
                .await,
            ids
        );
        let moved = sched.get_job(&job).await.unwrap();
        assert_eq!(moved.creator_identity_id.as_deref(), Some("carol"));
        assert_eq!(moved.edited_by.as_deref(), Some("admin-1"));
        assert!(
            !is_own(&moved),
            "it runs without carol's private context until she saves it"
        );
        assert!(
            sched.get_job(&reminder).await.unwrap().names_any(&alice()),
            "only the job named moves"
        );
    }

    #[tokio::test]
    async fn forgetting_jobs_removes_them_with_their_run_history() {
        let sched = test_scheduler().await;
        let (job, reminder) = owned_jobs(&sched, "alice").await;
        let run = |job_id: &str| JobRunRecord {
            job_id: job_id.into(),
            job_name: "n".into(),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            duration_ms: 1,
            success: true,
            output: "private output".into(),
            delivered: true,
            note: None,
        };
        sched.push_history(run(&job)).await;
        sched.push_history(run(&reminder)).await;

        assert_eq!(sched.forget_jobs(std::slice::from_ref(&job)).await, 1);
        assert!(sched.get_job(&job).await.is_none());
        assert!(sched.job_run_history(&job).await.is_empty());
        assert_eq!(sched.job_run_history(&reminder).await.len(), 1);
        let persisted =
            std::fs::read_to_string(sched.data_dir.join("scheduler").join("run-history.jsonl"))
                .unwrap();
        assert!(!persisted.contains(&job), "{persisted}");
        assert!(persisted.contains(&reminder));
        assert_eq!(sched.forget_jobs(&[job]).await, 0);
    }

    fn turn_payload() -> JobPayload {
        JobPayload::AgentTurn {
            message: "go".into(),
            channel: None,
            channel_id: None,
            thread_id: None,
            model: None,
            user_id: None,
            max_rounds: None,
            agent_owns_delivery: false,
            tools: None,
        }
    }

    fn hourly() -> Schedule {
        Schedule::Every {
            every_secs: 3600,
            anchor: None,
        }
    }

    /// A stored job as the tool once created it: pinned to a session named
    /// by the key that is also its delivery channel.
    fn pinned_to_its_delivery_channel() -> JobPayload {
        JobPayload::AgentTurn {
            message: "check the queue".into(),
            channel: Some("D123".into()),
            channel_id: Some("D123".into()),
            thread_id: Some("17.5".into()),
            model: None,
            user_id: None,
            max_rounds: None,
            agent_owns_delivery: false,
            tools: Some(vec!["queue".into()]),
        }
    }

    /// An edit to `session` its owner has approved the cost of.
    fn edit_session(session: SessionMode) -> JobEdit {
        JobEdit {
            session: Some(session),
            confirmed: true,
            ..Default::default()
        }
    }

    /// `job_id` as a store holds it once pinned, whatever the config allows.
    async fn store_pinned(sched: &Scheduler, job_id: &str) -> String {
        let mut jobs = sched.jobs.write().await;
        let job = jobs.iter_mut().find(|j| j.id == job_id).unwrap();
        edit_session(SessionMode::Pinned).apply(job, &Utc::now(), chrono_tz::UTC);
        job.pinned_session().expect("pinned")
    }

    fn session_channel(job: &ScheduledJob) -> Option<&str> {
        match &job.payload {
            JobPayload::AgentTurn { channel, .. } => channel.as_deref(),
            _ => None,
        }
    }

    /// A scheduler on a mock model whose deliveries land in the returned log.
    async fn delivering_scheduler(
        data_dir: &std::path::Path,
        configure: impl FnOnce(&mut workforce_common::settings::WorkforceConfig),
    ) -> (
        Arc<Scheduler>,
        Arc<std::sync::Mutex<Vec<String>>>,
        wiremock::MockServer,
    ) {
        let (service, server) =
            crate::serve::testing::service_on_mock_llm_with(data_dir, configure).await;
        let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Arc::clone(&delivered);
        let delivery: DeliveryCallback = Arc::new(move |_, text| {
            log.lock().unwrap().push(text);
            Box::pin(async {}) as _
        });
        let sched = Scheduler::with_delivery_and_timezone(service, delivery, chrono_tz::UTC);
        (Arc::new(sched), delivered, server)
    }

    /// The scheduler sessions holding at least one run, as (channel, messages).
    fn run_sessions(sched: &Scheduler) -> Vec<(String, usize)> {
        let sessions = &sched.service().sessions;
        let mut runs: Vec<(String, usize)> = sessions
            .list_for_owner("scheduler", 100)
            .unwrap()
            .into_iter()
            .filter_map(|shell| sessions.get(&shell.id).unwrap())
            .map(|session| (session.channel.clone(), session.messages.len()))
            .collect();
        runs.sort();
        runs
    }

    async fn run_twice(sched: &Scheduler, id: &str) -> Vec<JobRunRecord> {
        let first = sched.run_job_now(id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        vec![first, sched.run_job_now(id).await.unwrap()]
    }

    #[tokio::test]
    async fn a_pinned_job_switched_to_fresh_runs_in_a_new_session_each_time_and_keeps_its_delivery()
    {
        let tmp = tempfile::tempdir().unwrap();
        let (sched, delivered, _server) = delivering_scheduler(tmp.path(), |_| {}).await;
        let id = sched
            .add_job_owned(
                "queue",
                None,
                hourly(),
                pinned_to_its_delivery_channel(),
                Some("owner".into()),
                None,
            )
            .await
            .unwrap();
        sched.update_job_run_as(&id, Some("runner".into())).await;
        sched.update_job_bill_to(&id, Some("payer".into())).await;
        let before = sched
            .update_job(&id, edit_session(SessionMode::Pinned), "admin")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.session_mode(), Some(SessionMode::Pinned));
        let pin = before.pinned_session().unwrap();

        run_twice(&sched, &id).await;
        let pinned = run_sessions(&sched);
        assert_eq!(pinned.len(), 1, "both runs share the pinned session");
        assert_eq!(pinned[0].0, pin);

        let after = sched
            .update_job(&id, edit_session(SessionMode::Fresh), "admin")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.session_mode(), Some(SessionMode::Fresh));
        let JobPayload::AgentTurn {
            channel_id,
            thread_id,
            agent_owns_delivery,
            tools,
            ..
        } = &after.payload
        else {
            panic!("an agent turn");
        };
        assert_eq!(
            (
                channel_id.as_deref(),
                thread_id.as_deref(),
                *agent_owns_delivery
            ),
            (Some("D123"), Some("17.5"), false)
        );
        assert_eq!(tools.as_deref(), Some(&["queue".to_string()][..]));
        assert_eq!(
            (
                after.creator_identity_id,
                after.run_as_identity_id,
                after.bill_to_identity_id
            ),
            (
                before.creator_identity_id,
                before.run_as_identity_id,
                before.bill_to_identity_id
            )
        );

        let records = run_twice(&sched, &id).await;
        assert!(records
            .iter()
            .all(|r| r.success && r.delivered && r.note.is_none()));
        assert_eq!(delivered.lock().unwrap().len(), 4, "every run delivered");
        let fresh: Vec<(String, usize)> = run_sessions(&sched)
            .into_iter()
            .filter(|(channel, _)| *channel != pin)
            .collect();
        assert_eq!(fresh.len(), 2, "one session per fresh run: {fresh:?}");
        assert!(fresh
            .iter()
            .all(|(channel, _)| channel.starts_with(&format!("scheduler:{id}:"))));
        let one_run = fresh[0].1;
        assert!(
            fresh.iter().all(|(_, messages)| *messages == one_run),
            "no fresh run carries another's transcript: {fresh:?}"
        );
        assert_eq!(
            run_sessions(&sched)
                .into_iter()
                .find(|(channel, _)| *channel == pin)
                .map(|(_, messages)| messages),
            Some(pinned[0].1),
            "the old pinned session is left as it was"
        );
    }

    #[tokio::test]
    async fn a_job_stored_with_a_conversations_key_keeps_its_persons_session_until_it_is_pinned_again(
    ) {
        let sched = test_scheduler().await;
        let mut jobs = Vec::new();
        for owner in ["alice", "bob"] {
            let stored = pinned_to_its_delivery_channel();
            let id = sched
                .add_job_owned("queue", None, hourly(), stored, Some(owner.into()), None)
                .await
                .unwrap();
            jobs.push(sched.get_job(&id).await.unwrap());
        }
        let kept: Vec<String> = jobs
            .iter()
            .filter_map(ScheduledJob::pinned_session)
            .collect();
        assert_eq!(kept, ["scheduler:alice:D123", "scheduler:bob:D123"]);
        assert!(jobs
            .iter()
            .all(|job| job.session_mode() == Some(SessionMode::Pinned)));
        assert!(
            kept.iter().all(|session| is_job_session(session)),
            "bounded like a session minted for the job"
        );

        let id = &jobs[0].id;
        let still = sched
            .update_job(id, edit_session(SessionMode::Pinned), "admin")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            still.pinned_session().as_deref(),
            Some("scheduler:alice:D123")
        );
        sched
            .update_job(id, edit_session(SessionMode::Fresh), "admin")
            .await
            .unwrap();
        let again = sched
            .update_job(id, edit_session(SessionMode::Pinned), "admin")
            .await
            .unwrap()
            .unwrap();
        let minted = session_channel(&again).expect("a key of its own");
        assert!(minted.starts_with(&format!("scheduler:pinned:{id}:")));
        assert_eq!(
            again.pinned_session(),
            Some(format!("scheduler:alice:{minted}"))
        );
    }

    #[tokio::test]
    async fn switching_a_job_between_pinned_and_fresh_keeps_it_its_persons_unless_another_shares_the_session(
    ) {
        use crate::serve::RunAuthor::{Other, Principal};
        let sched = test_scheduler().await;
        let alices = sched
            .add_job_owned(
                "own",
                None,
                hourly(),
                turn_payload(),
                Some("alice".into()),
                None,
            )
            .await
            .unwrap();
        // An admin's job run as her, stored by an earlier version continuing
        // her web conversation.
        let mut ios = turn_payload();
        if let JobPayload::AgentTurn { channel, .. } = &mut ios {
            *channel = Some("ios".into());
        }
        let on_behalf = sched
            .add_job_owned("theirs", None, hourly(), ios, Some("admin".into()), None)
            .await
            .unwrap();
        sched
            .update_job_run_as(&on_behalf, Some("alice".into()))
            .await;
        let author = |id: &str| {
            let (sched, id) = (&sched, id.to_string());
            async move {
                let jobs = sched.list_jobs().await;
                let job = jobs.iter().find(|j| j.id == id).unwrap();
                run_author(job, &jobs)
            }
        };
        assert_eq!(author(&alices).await, Principal);
        assert_eq!(author(&on_behalf).await, Other);

        sched
            .update_job(&alices, edit_session(SessionMode::Pinned), "alice")
            .await
            .unwrap();
        assert_eq!(
            author(&alices).await,
            Principal,
            "her pin is a session minted for her job, which nobody else's continues"
        );

        let mut joined = sched.get_job(&alices).await.unwrap();
        if let JobPayload::AgentTurn { channel, .. } = &mut joined.payload {
            *channel = Some("ios".into());
        }
        sched.jobs.write().await[0] = joined;
        assert_eq!(
            author(&alices).await,
            Other,
            "a session another person's job writes to is not hers alone"
        );

        sched
            .update_job(&alices, edit_session(SessionMode::Fresh), "alice")
            .await
            .unwrap();
        assert_eq!(author(&alices).await, Principal);

        sched
            .update_job(&alices, edit_session(SessionMode::Pinned), "admin")
            .await
            .unwrap();
        assert_eq!(author(&alices).await, Other, "an admin pinned it");
        sched
            .update_job(&alices, JobEdit::default(), "alice")
            .await
            .unwrap();
        assert_eq!(
            author(&alices).await,
            Principal,
            "until she saves it herself"
        );
    }

    #[tokio::test]
    async fn a_pinned_job_past_the_message_ceiling_carries_on_in_a_successor_and_still_delivers() {
        let tmp = tempfile::tempdir().unwrap();
        let (sched, delivered, _server) = delivering_scheduler(tmp.path(), |config| {
            config.compaction.chat_rollover_max_messages = 4;
        })
        .await;
        let id = sched
            .add_job_owned(
                "queue",
                None,
                hourly(),
                turn_payload(),
                Some("owner".into()),
                None,
            )
            .await
            .unwrap();
        sched.update_job_run_as(&id, Some("runner".into())).await;
        let pin = store_pinned(&sched, &id).await;

        let mut records = run_twice(&sched, &id).await;
        records.extend(run_twice(&sched, &id).await);

        assert!(records
            .iter()
            .all(|r| r.success && r.delivered && r.note.is_none()));
        assert_eq!(delivered.lock().unwrap().len(), 4, "every run delivered");
        let sessions = &sched.service().sessions;
        let thread: Vec<_> = sessions
            .list_for_owner("scheduler", 100)
            .unwrap()
            .into_iter()
            .filter_map(|shell| sessions.get(&shell.id).unwrap())
            .collect();
        assert!(thread.len() > 1, "the session rolled over");
        assert!(
            thread.iter().all(|session| session.channel == pin),
            "every successor is the job's own session"
        );
        assert_eq!(
            thread.iter().filter(|session| !session.is_sealed()).count(),
            1,
            "one session is live, the rest are kept sealed"
        );
        assert!(thread.iter().all(|session| session.messages.len() <= 6));
        let job = sched.get_job(&id).await.unwrap();
        assert_eq!(job.effective_run_as_identity(), Some("runner"));
        assert_eq!(job.pinned_session(), Some(pin));
    }

    #[tokio::test]
    async fn a_pinned_job_fired_while_a_pinned_run_is_going_waits_for_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (sched, _delivered, _server) = delivering_scheduler(tmp.path(), |_| {}).await;
        let mut ids = Vec::new();
        for name in ["pinned", "fresh"] {
            ids.push(
                sched
                    .add_job_owned(name, None, hourly(), turn_payload(), Some("u".into()), None)
                    .await
                    .unwrap(),
            );
        }
        store_pinned(&sched, &ids[0]).await;
        let going = sched.pinned_run.lock().await;

        let fired = tokio::spawn({
            let (sched, id) = (Arc::clone(&sched), ids[0].clone());
            async move { sched.run_job_now(&id).await }
        });
        assert!(sched.run_job_now(&ids[1]).await.unwrap().success);
        assert!(run_sessions(&sched)
            .iter()
            .all(|(channel, _)| !channel.contains(PINNED_SESSION_PREFIX)));
        assert!(!fired.is_finished());

        drop(going);
        assert!(fired.await.unwrap().unwrap().success);
        assert!(run_sessions(&sched)
            .iter()
            .any(|(channel, _)| channel.contains(PINNED_SESSION_PREFIX)));
    }

    #[test]
    fn a_kept_session_is_its_persons_whatever_key_the_job_was_stored_with() {
        let minted = pinned_session_channel("fixture");
        let stored_with = |owner: &str, channel: &str| {
            let mut job = job_fixture(Some(owner), None, turn_payload());
            if let JobPayload::AgentTurn {
                channel: stored, ..
            } = &mut job.payload
            {
                *stored = Some(channel.to_string());
            }
            job.pinned_session().unwrap()
        };
        for key in [minted.as_str(), "web", "D123"] {
            assert_eq!(stored_with("alice", key), format!("scheduler:alice:{key}"));
            assert_ne!(stored_with("alice", key), stored_with("bob", key), "{key}");
        }
        assert_ne!(minted, pinned_session_channel("fixture"), "minted per pin");
        assert!(
            !continues_conversation(&stored_with("alice", &minted)),
            "a minted pin starts empty and continues no conversation"
        );
        assert!(continues_conversation(&stored_with("alice", "ios")));
    }

    #[tokio::test]
    async fn pinning_gives_a_job_a_session_of_its_own_and_pinning_again_a_new_one() {
        let sched = test_scheduler().await;
        let mut ids = Vec::new();
        for owner in ["alice", "bob"] {
            ids.push(
                sched
                    .add_job_owned(
                        "watch",
                        None,
                        hourly(),
                        turn_payload(),
                        Some(owner.into()),
                        None,
                    )
                    .await
                    .unwrap(),
            );
        }
        let mut pins = Vec::new();
        for id in &ids {
            let job = sched
                .update_job(id, edit_session(SessionMode::Pinned), "admin")
                .await
                .unwrap()
                .unwrap();
            pins.push(session_channel(&job).expect("pinned").to_string());
        }
        assert_ne!(pins[0], pins[1], "two jobs never share a session");
        assert!(pins.iter().all(|pin| pin.starts_with("scheduler:pinned:")));

        let kept = sched
            .update_job(&ids[0], edit_session(SessionMode::Pinned), "admin")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session_channel(&kept), Some(pins[0].as_str()));

        sched
            .update_job(&ids[0], edit_session(SessionMode::Fresh), "admin")
            .await
            .unwrap();
        let again = sched
            .update_job(&ids[0], edit_session(SessionMode::Pinned), "admin")
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            session_channel(&again),
            Some(pins[0].as_str()),
            "a job pinned again does not return to the transcript it left"
        );
    }

    #[test]
    fn a_stored_job_reads_as_pinned_exactly_when_its_payload_names_a_session() {
        let stored = |channel: &str| {
            format!(
                r#"{{"id":"j","name":"n","schedule":{{"kind":"every","every_secs":3600}},
                    "payload":{{"kind":"agent_turn","message":"m"{channel}}},
                    "enabled":true,"created_at":"2026-01-01T00:00:00Z",
                    "last_run":null,"next_run":null,"run_count":7}}"#
            )
        };
        let job = |channel: &str| serde_json::from_str::<ScheduledJob>(&stored(channel)).unwrap();
        assert_eq!(
            job(r#","channel":"scheduler:pinned:j:6f1d""#).session_mode(),
            Some(SessionMode::Pinned)
        );
        assert_eq!(
            job(r#","channel":"D123""#).session_mode(),
            Some(SessionMode::Pinned)
        );
        assert_eq!(job("").session_mode(), Some(SessionMode::Fresh));
        assert!(
            !serde_json::to_string(&job("")).unwrap().contains("session"),
            "the mode is derived, never stored beside the payload"
        );
        let reminder = job_fixture(
            None,
            None,
            JobPayload::Reminder {
                text: "r".into(),
                user_id: "u".into(),
            },
        );
        assert_eq!(reminder.session_mode(), None);
    }

    #[test]
    fn a_rest_edit_names_a_mode_never_a_session_and_a_job_is_served_with_its_mode() {
        let session = |body: &str| serde_json::from_str::<JobEdit>(body).map(|edit| edit.session);
        assert_eq!(
            session(r#"{"session":"fresh"}"#).unwrap(),
            Some(SessionMode::Fresh)
        );
        assert_eq!(
            session(r#"{"session":"pinned"}"#).unwrap(),
            Some(SessionMode::Pinned)
        );
        assert_eq!(session(r#"{"enabled":false}"#).unwrap(), None);
        assert!(session(r#"{"session":"D123"}"#).is_err());

        let served = |payload| crate::api::serialize_job(&job_fixture(Some("u"), None, payload));
        let mut pinned = job_fixture(Some("u"), None, turn_payload());
        edit_session(SessionMode::Pinned).apply(&mut pinned, &Utc::now(), chrono_tz::UTC);
        assert_eq!(crate::api::serialize_job(&pinned)["session"], "pinned");
        assert_eq!(
            served(pinned_to_its_delivery_channel())["session"],
            "pinned"
        );
        assert_eq!(served(turn_payload())["session"], "fresh");
        assert!(served(reminder_payload())["session"].is_null());
    }

    #[tokio::test]
    async fn a_listed_job_carries_what_its_runs_have_spent() {
        let sched = test_scheduler().await;
        let id = sched
            .add_job_owned("t", None, hourly(), turn_payload(), Some("u".into()), None)
            .await
            .unwrap();
        let job = sched.get_job(&id).await.unwrap();
        let spent =
            |sched: &Scheduler| crate::api::serialize_listed_job(sched, &job)["spent_usd"].clone();
        assert_eq!(spent(&sched), 0.0);
        for (cost, task) in [
            (1.5, format!("scheduler:{id}:1")),
            (9.0, "scheduler:other:1".into()),
        ] {
            sched
                .service()
                .telemetry_store
                .record_cost(&workforce_common::persistence::CostRecord {
                    agent_id: "a".to_string(),
                    model: "m".to_string(),
                    cost_usd: cost,
                    task_id: Some(task),
                    ..Default::default()
                })
                .unwrap();
        }
        assert_eq!(spent(&sched), 1.5);
        assert_eq!(job.run_count, 0);
    }

    #[tokio::test]
    async fn an_edit_is_refused_whole_when_any_part_of_it_is() {
        let sched = test_scheduler().await;
        let id = sched
            .add_job_owned(
                "r",
                None,
                hourly(),
                reminder_payload(),
                Some("u".into()),
                None,
            )
            .await
            .unwrap();
        let refused = sched
            .update_job(
                &id,
                JobEdit {
                    name: Some("renamed".into()),
                    session: Some(SessionMode::Pinned),
                    ..Default::default()
                },
                "admin",
            )
            .await;
        assert!(refused.is_err(), "a reminder runs in no session");
        for visibility in [VISIBILITY_GLOBAL, "everyone"] {
            let refused = sched
                .update_job(
                    &id,
                    JobEdit {
                        name: Some("renamed".into()),
                        visibility: Some(visibility.into()),
                        ..Default::default()
                    },
                    "admin",
                )
                .await;
            assert!(refused.is_err(), "{visibility}");
        }
        assert_eq!(sched.get_job(&id).await.unwrap().name, "r");
    }

    #[tokio::test]
    async fn an_edit_replaces_a_jobs_tools_and_an_empty_list_restores_them_all() {
        let sched = test_scheduler().await;
        let id = sched
            .add_job_owned("t", None, hourly(), turn_payload(), Some("u".into()), None)
            .await
            .unwrap();
        let tools_after = |tools: Vec<&str>| {
            let sched = Arc::clone(&sched);
            let id = id.clone();
            let tools = tools.into_iter().map(str::to_string).collect();
            async move {
                let job = sched
                    .update_job(
                        &id,
                        JobEdit {
                            tools: Some(tools),
                            ..Default::default()
                        },
                        "admin",
                    )
                    .await
                    .unwrap()
                    .unwrap();
                match job.payload {
                    JobPayload::AgentTurn { tools, .. } => tools,
                    _ => panic!("an agent turn"),
                }
            }
        };
        assert_eq!(
            tools_after(vec!["github"]).await,
            Some(vec!["github".to_string()])
        );
        assert_eq!(tools_after(vec![]).await, None);
    }

    #[tokio::test]
    async fn where_pinning_is_switched_off_a_pinned_recurring_job_runs_fresh_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let (sched, delivered, _server) = delivering_scheduler(tmp.path(), |config| {
            config.scheduler.pinned_sessions = false;
        })
        .await;
        let id = sched
            .add_job_owned(
                "queue",
                None,
                hourly(),
                turn_payload(),
                Some("owner".into()),
                None,
            )
            .await
            .unwrap();
        let pin = store_pinned(&sched, &id).await;

        let records = run_twice(&sched, &id).await;

        assert!(records
            .iter()
            .all(|r| r.success && r.note.as_deref() == Some(RAN_FRESH_BY_POLICY)));
        assert_eq!(delivered.lock().unwrap().len(), 2);
        let sessions = run_sessions(&sched);
        assert_eq!(sessions.len(), 2, "{sessions:?}");
        assert!(sessions.iter().all(|(channel, _)| *channel != pin));
        assert_eq!(
            sched.get_job(&id).await.unwrap().session_mode(),
            Some(SessionMode::Pinned),
            "the stored job is untouched: allowing pinning again restores it"
        );

        let fresh = sched
            .add_job_owned(
                "w",
                None,
                hourly(),
                turn_payload(),
                Some("owner".into()),
                None,
            )
            .await
            .unwrap();
        assert!(sched
            .update_job(&fresh, edit_session(SessionMode::Pinned), "admin")
            .await
            .is_err());
        let once = sched
            .add_job_owned(
                "once",
                None,
                Schedule::At {
                    at: Utc::now() + chrono::Duration::hours(1),
                },
                turn_payload(),
                Some("owner".into()),
                None,
            )
            .await
            .unwrap();
        assert!(
            sched
                .update_job(&once, edit_session(SessionMode::Pinned), "admin")
                .await
                .is_ok(),
            "a one-shot cannot grow a session"
        );
    }

    /// A store other instances write too, holding encoded rows and applying
    /// each change as the Postgres store does.
    #[derive(Default)]
    struct SharedStore(std::sync::Mutex<Vec<serde_json::Value>>);

    impl SharedStore {
        /// Write `job` whole, as a peer instance would.
        fn peer_writes(&self, job: &ScheduledJob) {
            let mut rows = self.0.lock().unwrap();
            rows.retain(|row| row["id"] != job.id.as_str());
            rows.push(serde_json::to_value(job).unwrap());
        }

        fn merge(&self, job: &ScheduledJob, fields: &[&str]) {
            let definition = serde_json::to_value(job).unwrap();
            let mut rows = self.0.lock().unwrap();
            match rows.iter_mut().find(|row| row["id"] == job.id.as_str()) {
                Some(row) => {
                    let written = crate::scheduler_store::part_of(&definition, fields, true);
                    let row = row.as_object_mut().unwrap();
                    row.extend(written.as_object().unwrap().clone());
                }
                None => rows.push(definition),
            }
        }

        fn merge_while(&self, job: &ScheduledJob, fields: &[&str], flag: &str) {
            let set = self
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|row| row["id"] == job.id.as_str() && row[flag] == true);
            if set {
                self.merge(job, fields);
            }
        }
    }

    use crate::scheduler_store::JobStore;

    #[async_trait::async_trait]
    impl JobStore for SharedStore {
        async fn load_all(&self) -> eyre::Result<Vec<ScheduledJob>> {
            let rows = self.0.lock().unwrap().clone();
            Ok(rows
                .into_iter()
                .map(|row| serde_json::from_value(row).unwrap())
                .collect())
        }

        async fn persist(
            &self,
            all: &[ScheduledJob],
            change: crate::scheduler_store::JobChange<'_>,
        ) -> eyre::Result<()> {
            use crate::scheduler_store::JobChange;
            match change {
                JobChange::Upserted(job) => self.peer_writes(job),
                JobChange::Refreshed(_) => eyre::bail!("no test here refreshes a definition"),
                JobChange::Fields(job, fields) => self.merge(job, fields),
                JobChange::FieldsWhile(job, fields, flag) => self.merge_while(job, fields, flag),
                JobChange::Removed(ids) => self
                    .0
                    .lock()
                    .unwrap()
                    .retain(|row| !ids.iter().any(|id| row["id"] == id.as_str())),
                JobChange::Replaced => all.iter().for_each(|job| self.peer_writes(job)),
            }
            Ok(())
        }

        fn is_shared(&self) -> bool {
            true
        }
    }

    /// A scheduler on a store shared with a peer, and that store.
    async fn follower() -> (Arc<Scheduler>, Arc<SharedStore>) {
        let sched = test_scheduler().await;
        let store = Arc::new(SharedStore::default());
        assert!(sched
            .store
            .set(Arc::clone(&store) as Arc<dyn JobStore>)
            .is_ok());
        (sched, store)
    }

    /// A suspended person's reminder on a store shared with a peer.
    async fn held_on_a_shared_store() -> (Arc<Scheduler>, Arc<SharedStore>, String, Vec<String>) {
        let (sched, store) = follower().await;
        let id = sched
            .add_job_owned(
                "t",
                None,
                hourly(),
                reminder_payload(),
                Some("u".into()),
                None,
            )
            .await
            .unwrap();
        let names = vec!["u".to_string()];
        assert_eq!(sched.hold_for_suspension(&names, true).await.len(), 1);
        (sched, store, id, names)
    }

    #[tokio::test]
    async fn a_release_on_a_shared_store_clears_the_hold_it_stored() {
        let (sched, store, _, _) = held_on_a_shared_store().await;
        assert!(store.load_all().await.unwrap()[0].paused_by_suspension);

        assert_eq!(
            sched.release_suspension_holds(&|_| false, true).await.len(),
            1
        );
        let stored = store.load_all().await.unwrap().remove(0);
        assert!(stored.enabled && !stored.paused_by_suspension);
        assert!(sched
            .release_suspension_holds(&|_| false, true)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn a_held_job_switched_off_by_hand_on_a_shared_store_stays_off() {
        let (sched, store, id, _) = held_on_a_shared_store().await;
        sched
            .update_job(
                &id,
                JobEdit {
                    enabled: Some(false),
                    ..JobEdit::default()
                },
                "admin",
            )
            .await
            .unwrap();
        assert!(!store.load_all().await.unwrap()[0].paused_by_suspension);
        assert!(sched
            .release_suspension_holds(&|_| false, true)
            .await
            .is_empty());
        assert!(!store.load_all().await.unwrap()[0].enabled);
    }

    #[tokio::test]
    async fn an_edit_on_a_follower_keeps_what_the_leader_wrote_since_its_snapshot() {
        let (sched, store) = follower().await;
        let id = sched
            .add_job_owned("t", None, hourly(), turn_payload(), Some("u".into()), None)
            .await
            .unwrap();
        // The leader ran the job, narrowed its tools and paused it since this
        // follower last read the store.
        let mut led = sched.get_job(&id).await.unwrap();
        led.run_count = 7;
        led.last_run = Some(Utc::now());
        led.next_run = Some(Utc::now() + chrono::Duration::hours(1));
        led.budget_usd = Some(1.0);
        led.enabled = false;
        led.edited_by = Some("monitor".into());
        if let JobPayload::AgentTurn { tools, .. } = &mut led.payload {
            *tools = Some(vec!["queue".into()]);
        }
        store.peer_writes(&led);
        assert_eq!(sched.get_job(&id).await.unwrap().run_count, 0, "stale");

        let edited = sched
            .update_job(
                &id,
                JobEdit {
                    name: Some("renamed".into()),
                    ..edit_session(SessionMode::Pinned)
                },
                "admin",
            )
            .await
            .unwrap()
            .unwrap();

        let stored = store.load_all().await.unwrap().remove(0);
        for job in [&edited, &stored] {
            assert_eq!(job.name, "renamed");
            assert_eq!(job.session_mode(), Some(SessionMode::Pinned));
            assert_eq!(
                (job.run_count, job.last_run, job.next_run),
                (7, led.last_run, led.next_run)
            );
            assert_eq!((job.budget_usd, job.enabled), (Some(1.0), false));
            assert_eq!(
                job.edited_by.as_deref(),
                Some("admin"),
                "the edit's editor is written"
            );
            assert!(
                matches!(&job.payload, JobPayload::AgentTurn { tools: Some(tools), .. } if tools == &["queue"]),
                "the edit is applied to the payload the store holds"
            );
        }
    }

    #[tokio::test]
    async fn a_runs_bookkeeping_keeps_an_edit_a_peer_made_since_the_leaders_snapshot() {
        let (sched, store) = follower().await;
        sched.set_ready();
        let id = sched
            .add_job_owned(
                "t",
                None,
                hourly(),
                reminder_payload(),
                Some("u".into()),
                None,
            )
            .await
            .unwrap();
        // Its owner renamed and paused it on a peer; this instance read the
        // store just before, and the job is due as it last saw it.
        let mut edited = sched.get_job(&id).await.unwrap();
        edited.name = "renamed".into();
        edited.enabled = false;
        edited.edited_by = Some("admin".into());
        store.peer_writes(&edited);
        *sched.last_store_refresh.lock().unwrap() = Some(std::time::Instant::now());
        sched.jobs.write().await[0].next_run = Some(Utc::now() - chrono::Duration::seconds(1));

        sched.tick().await;

        let stored = store.load_all().await.unwrap().remove(0);
        assert_eq!(stored.run_count, 1, "the run is counted");
        assert_eq!((stored.name.as_str(), stored.enabled), ("renamed", false));
        assert_eq!(
            stored.edited_by.as_deref(),
            Some("admin"),
            "a run never clears it"
        );
    }

    #[tokio::test]
    async fn upsert_then_sweep_by_id_collapses_same_named_duplicates() {
        let sched = test_scheduler().await;
        let schedule = || Schedule::Cron {
            expr: "0 8 * * 1-5".to_string(),
            tz: Some("Europe/London".to_string()),
        };
        let payload = || JobPayload::WorkflowStart {
            workflow_id: "wf".to_string(),
            payload: serde_json::json!({}),
            channel_id: Some("C1".to_string()),
            thread_id: None,
        };

        // Every pre-fix reload added a fresh row under the same name.
        for _ in 0..3 {
            sched
                .add_job_with_desc("workflow-schedule:wf:0", None, schedule(), payload())
                .await
                .unwrap();
        }
        assert_eq!(sched.list_jobs().await.len(), 3);

        let kept = sched
            .upsert_job("workflow-schedule:wf:0", None, schedule(), payload())
            .await
            .unwrap();

        // The sweep itself is `workflow_runtime::stale_schedule_job_ids`, covered
        // there; this asserts the scheduler primitives it composes.
        let live = std::collections::HashSet::from([kept.clone()]);
        let jobs = sched.list_jobs().await;
        let stale = crate::workflow_runtime::stale_schedule_job_ids(
            jobs.iter().map(|j| (j.id.as_str(), j.name.as_str())),
            &live,
        );
        for id in &stale {
            sched.remove_job(id).await;
        }

        let remaining = sched.list_jobs().await;
        assert_eq!(remaining.len(), 1, "one row per trigger must survive");
        assert_eq!(remaining[0].id, kept);
    }

    #[tokio::test]
    async fn remove_workflow_start_jobs_sweeps_only_workflow_jobs() {
        let sched = test_scheduler().await;
        let schedule = || Schedule::Cron {
            expr: "0 8 * * 1-5".to_string(),
            tz: None,
        };
        let wf_payload = |id: &str| JobPayload::WorkflowStart {
            workflow_id: id.to_string(),
            payload: serde_json::json!({}),
            channel_id: Some("C1".to_string()),
            thread_id: None,
        };
        for id in ["wf", "wf2"] {
            sched
                .add_job_with_desc(
                    &format!("workflow-schedule:{id}:0"),
                    None,
                    schedule(),
                    wf_payload(id),
                )
                .await
                .unwrap();
        }
        sched
            .add_job_with_desc("reminder", None, schedule(), turn_payload())
            .await
            .unwrap();

        assert_eq!(sched.remove_workflow_start_jobs().await, 2);
        let remaining = sched.list_jobs().await;
        assert_eq!(
            remaining.len(),
            1,
            "non-workflow jobs must survive the sweep"
        );
        assert!(matches!(remaining[0].payload, JobPayload::AgentTurn { .. }));
        assert_eq!(sched.remove_workflow_start_jobs().await, 0);
    }

    #[tokio::test]
    async fn upsert_job_keeps_a_pending_occurrence_when_the_cadence_is_unchanged() {
        let sched = test_scheduler().await;
        let schedule = || Schedule::Cron {
            expr: "0 8 * * 1-5".to_string(),
            tz: None,
        };
        let id = sched
            .upsert_job(
                "workflow-schedule:wf:0",
                None,
                schedule(),
                JobPayload::MonitorSweep,
            )
            .await
            .unwrap();
        let armed = sched.get_job(&id).await.unwrap().next_run;

        sched
            .upsert_job(
                "workflow-schedule:wf:0",
                None,
                schedule(),
                JobPayload::MonitorSweep,
            )
            .await
            .unwrap();
        assert_eq!(
            sched.get_job(&id).await.unwrap().next_run,
            armed,
            "a reload must not push a due job past its own occurrence"
        );

        sched
            .upsert_job(
                "workflow-schedule:wf:0",
                None,
                Schedule::Cron {
                    expr: "0 14 * * 1-5".to_string(),
                    tz: None,
                },
                JobPayload::MonitorSweep,
            )
            .await
            .unwrap();
        assert_ne!(
            sched.get_job(&id).await.unwrap().next_run,
            armed,
            "an edited cron must re-arm"
        );
    }

    #[tokio::test]
    async fn upsert_job_preserves_identity_and_run_count() {
        let sched = test_scheduler().await;
        let schedule = || Schedule::Cron {
            expr: "0 8 * * 1-5".to_string(),
            tz: None,
        };
        let first = sched
            .upsert_job(
                "workflow-schedule:wf:0",
                None,
                schedule(),
                JobPayload::MonitorSweep,
            )
            .await
            .unwrap();
        let second = sched
            .upsert_job(
                "workflow-schedule:wf:0",
                None,
                schedule(),
                JobPayload::MonitorSweep,
            )
            .await
            .unwrap();
        assert_eq!(first, second, "a reload must not mint a new job id");
        assert_eq!(sched.list_jobs().await.len(), 1);
    }

    #[tokio::test]
    async fn owned_recurring_agent_jobs_get_default_caps_system_jobs_do_not() {
        let sched = test_scheduler().await;
        let hourly = Schedule::Every {
            every_secs: 3600,
            anchor: None,
        };
        let owned = sched
            .add_job_owned(
                "owned",
                None,
                hourly.clone(),
                turn_payload(),
                Some("user-1".into()),
                None,
            )
            .await
            .unwrap();
        let owned = sched.get_job(&owned).await.unwrap();
        assert_eq!(owned.max_runs, Some(100));
        assert_eq!(owned.budget_usd, Some(25.0));
        assert!(owned.expires_at.is_some());

        // Creator-less registration (system boot path): no caps, no floor.
        let system = sched
            .add_job(
                "system",
                Schedule::Every {
                    every_secs: 120,
                    anchor: None,
                },
                JobPayload::MonitorSweep,
            )
            .await
            .unwrap();
        let system = sched.get_job(&system).await.unwrap();
        assert!(system.max_runs.is_none());
        assert!(system.expires_at.is_none());
    }

    #[tokio::test]
    async fn owned_schedule_below_floor_is_rejected_at_insert_update_and_upsert() {
        let sched = test_scheduler().await;
        let fast = Schedule::Every {
            every_secs: 30,
            anchor: None,
        };
        assert!(sched
            .add_job_owned(
                "fast",
                None,
                fast.clone(),
                turn_payload(),
                Some("u".into()),
                None
            )
            .await
            .is_err());

        let ok = sched
            .add_job_owned(
                "slow",
                None,
                Schedule::Every {
                    every_secs: 3600,
                    anchor: None,
                },
                turn_payload(),
                Some("u".into()),
                None,
            )
            .await
            .unwrap();
        assert!(sched
            .update_job(
                &ok,
                JobEdit {
                    schedule: Some(fast.clone()),
                    ..Default::default()
                },
                "admin",
            )
            .await
            .is_err());
        // The upsert-by-name in-place branch must validate too — it bypasses
        // insert_job entirely.
        assert!(sched
            .upsert_job("slow", None, fast, turn_payload())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn cap_breach_catches_expiry_run_limit_and_leaves_fresh_jobs_alone() {
        let sched = test_scheduler().await;
        let id = sched
            .add_job_owned(
                "capped",
                None,
                Schedule::Every {
                    every_secs: 3600,
                    anchor: None,
                },
                turn_payload(),
                Some("u".into()),
                None,
            )
            .await
            .unwrap();
        let now = Utc::now();
        let mut job = sched.get_job(&id).await.unwrap();
        assert!(sched.cap_breach(&job, &now).is_none());

        job.run_count = 100;
        assert!(sched
            .cap_breach(&job, &now)
            .is_some_and(|r| r.contains("run limit")));

        job.run_count = 0;
        job.expires_at = Some(now - chrono::Duration::hours(1));
        assert!(sched
            .cap_breach(&job, &now)
            .is_some_and(|r| r.contains("expired")));
    }

    #[tokio::test]
    async fn cap_breach_catches_cumulative_job_spend() {
        let sched = test_scheduler().await;
        let id = sched
            .add_job_owned(
                "spender",
                None,
                Schedule::Every {
                    every_secs: 3600,
                    anchor: None,
                },
                turn_payload(),
                Some("u".into()),
                None,
            )
            .await
            .unwrap();
        let job = sched.get_job(&id).await.unwrap();
        assert_eq!(job.budget_usd, Some(25.0));

        let record = |cost: f64, task_id: String| workforce_common::persistence::CostRecord {
            agent_id: "a".to_string(),
            model: "m".to_string(),
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_usd: cost,
            team_id: None,
            task_id: Some(task_id),
            task_type: None,
            ticket_id: None,
            identity_id: None,
            description: None,
            ..Default::default()
        };
        let service = sched.service();
        let store = &service.telemetry_store;
        store
            .record_cost(&record(20.0, format!("scheduler:{id}:1")))
            .unwrap();
        // Another job's spend must not count against this one.
        store
            .record_cost(&record(90.0, "scheduler:other-job:1".to_string()))
            .unwrap();
        // A subscription's estimated value is not money the job spent.
        store
            .record_cost(&workforce_common::persistence::CostRecord {
                billing: workforce_common::subscription::Billing::Subscription,
                ..record(90.0, format!("scheduler:{id}:sub"))
            })
            .unwrap();
        assert!(sched.cap_breach(&job, &Utc::now()).is_none());

        store
            .record_cost(&record(6.0, format!("scheduler:{id}:2")))
            .unwrap();
        assert!(sched
            .cap_breach(&job, &Utc::now())
            .is_some_and(|r| r.contains("budget")));
    }

    #[tokio::test]
    async fn legacy_agent_turn_consolidation_job_migrates_to_typed_payload() {
        let sched = test_scheduler().await;
        // Simulate a pre-upgrade persisted row: the old AgentTurn payload
        // under the exact registration name.
        sched
            .upsert_job(
                "memory-consolidation",
                None,
                Schedule::Every {
                    every_secs: 7200,
                    anchor: None,
                },
                turn_payload(),
            )
            .await
            .unwrap();

        let id =
            crate::memory_consolidation::register_consolidation_job(sched.as_ref(), Some(7200))
                .await
                .unwrap();
        let job = sched.get_job(&id).await.unwrap();
        assert!(matches!(
            job.payload,
            JobPayload::Consolidation { identity_id: None }
        ));
        assert_eq!(
            job.creator_identity_id.as_deref(),
            Some(crate::memory_consolidation::CONSOLIDATION_SYSTEM_IDENTITY)
        );
        // Registration is idempotent by name — one job, not two.
        let all = sched.list_jobs().await;
        assert_eq!(
            all.iter()
                .filter(|j| j.name == "memory-consolidation")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn tighten_job_caps_never_loosens() {
        let sched = test_scheduler().await;
        let id = sched
            .add_job_owned(
                "t",
                None,
                Schedule::Every {
                    every_secs: 3600,
                    anchor: None,
                },
                turn_payload(),
                Some("u".into()),
                None,
            )
            .await
            .unwrap();
        assert!(sched.tighten_job_caps(&id, Some(10), None, None).await);
        assert_eq!(sched.get_job(&id).await.unwrap().max_runs, Some(10));
        assert!(sched.tighten_job_caps(&id, Some(50), None, None).await);
        assert_eq!(sched.get_job(&id).await.unwrap().max_runs, Some(10));
    }

    #[test]
    fn consolidation_payload_round_trips() {
        let payload = JobPayload::Consolidation {
            identity_id: Some("user-1".to_string()),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["kind"], "consolidation");
        let back: JobPayload = serde_json::from_value(json).unwrap();
        assert!(matches!(
            back,
            JobPayload::Consolidation { identity_id: Some(id) } if id == "user-1"
        ));
    }

    #[test]
    fn next_run_every() {
        let now = Utc::now();
        let schedule = Schedule::Every {
            every_secs: 3600,
            anchor: None,
        };

        let next = next_run_time(&schedule, &now, DEFAULT_TZ).expect("should have next run");
        assert!(next > now);
    }

    #[test]
    fn next_run_cron_utc() {
        // "every minute" cron in UTC
        let now = Utc.with_ymd_and_hms(2026, 6, 15, 12, 30, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "* * * * *".to_string(),
            tz: None,
        };

        let next = next_run_time(&schedule, &now, DEFAULT_TZ).expect("should parse cron");
        assert!(next > now);
        // Should be at 12:31
        assert_eq!(next.minute(), 31);
    }

    #[test]
    fn next_run_cron_with_timezone() {
        // Cron "at 09:00 every day" in New York time
        let _ny: Tz = "America/New_York".parse().expect("valid tz");

        // At 12:00 UTC on a summer day (= 08:00 EDT), next 09:00 EDT = 13:00 UTC
        let now = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 9 * * *".to_string(),
            tz: Some("America/New_York".to_string()),
        };

        let next = next_run_time(&schedule, &now, DEFAULT_TZ).expect("should parse cron");
        assert!(next > now);
        // 09:00 EDT = 13:00 UTC
        assert_eq!(next.hour(), 13);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn next_run_cron_uses_default_tz() {
        // Cron with no explicit tz should use the default_tz
        let london: Tz = "Europe/London".parse().expect("valid tz");

        // 2026-01-15 (winter, GMT = UTC+0) at 08:30 UTC
        let now = Utc.with_ymd_and_hms(2026, 1, 15, 8, 30, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 9 * * *".to_string(),
            tz: None,
        };

        // With London as default, 09:00 GMT = 09:00 UTC in January
        let next = next_run_time(&schedule, &now, london).expect("should parse cron");
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn cron_invalid_expression_returns_none() {
        let now = Utc::now();
        let schedule = Schedule::Cron {
            expr: "not a cron".to_string(),
            tz: None,
        };
        assert!(next_run_time(&schedule, &now, DEFAULT_TZ).is_none());
    }

    #[test]
    fn job_serialization_round_trip() {
        let job = ScheduledJob {
            id: "test-123".to_string(),
            name: "daily report".to_string(),
            description: Some("Send daily summary".to_string()),
            schedule: Schedule::Cron {
                expr: "0 9 * * *".to_string(),
                tz: Some("Europe/London".to_string()),
            },
            payload: JobPayload::AgentTurn {
                message: "Generate report".to_string(),
                channel: Some("slack".to_string()),
                channel_id: None,
                thread_id: None,
                model: Some("claude-sonnet-4".to_string()),
                user_id: None,
                max_rounds: None,
                agent_owns_delivery: false,
                tools: None,
            },
            enabled: true,
            created_at: Utc::now(),
            last_run: None,
            next_run: Some(Utc::now()),
            run_count: 0,
            is_reminder: false,
            delivery_failures: 0,
            creator_identity_id: Some("user-abc".to_string()),
            run_as_identity_id: None,
            bill_to_identity_id: None,
            visibility: Some(VISIBILITY_PRIVATE.to_string()),
            max_runs: None,
            budget_usd: None,
            expires_at: None,
            deliver_to: None,
            edited_by: None,
            paused_by_suspension: false,
        };

        let json = serde_json::to_string(&job).unwrap();
        let deserialized: ScheduledJob = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "daily report");
        assert_eq!(
            deserialized.creator_identity_id.as_deref(),
            Some("user-abc")
        );
        assert_eq!(
            deserialized.description.as_deref(),
            Some("Send daily summary")
        );
    }

    fn job_fixture(
        creator: Option<&str>,
        visibility: Option<&str>,
        payload: JobPayload,
    ) -> ScheduledJob {
        let is_reminder = matches!(payload, JobPayload::Reminder { .. });
        ScheduledJob {
            id: "fixture".to_string(),
            name: "fixture".to_string(),
            description: None,
            schedule: Schedule::Cron {
                expr: "0 9 * * *".to_string(),
                tz: None,
            },
            payload,
            enabled: true,
            created_at: Utc::now(),
            last_run: None,
            next_run: None,
            run_count: 0,
            is_reminder,
            delivery_failures: 0,
            creator_identity_id: creator.map(str::to_string),
            run_as_identity_id: None,
            bill_to_identity_id: None,
            visibility: visibility.map(str::to_string),
            max_runs: None,
            budget_usd: None,
            expires_at: None,
            deliver_to: None,
            edited_by: None,
            paused_by_suspension: false,
        }
    }

    fn agent_turn() -> JobPayload {
        JobPayload::AgentTurn {
            message: "go".to_string(),
            channel: None,
            channel_id: None,
            thread_id: None,
            model: None,
            user_id: None,
            max_rounds: None,
            agent_owns_delivery: false,
            tools: None,
        }
    }

    fn turn_with(edit: fn(&mut JobPayload)) -> JobPayload {
        let mut payload = agent_turn();
        edit(&mut payload);
        payload
    }

    #[test]
    fn a_persons_own_private_job_is_theirs_whatever_it_posts_or_uses() {
        use crate::serve::RunAuthor::Principal;
        let own = |payload| job_fixture(Some("alice"), None, payload);
        let mut run_as_self = own(agent_turn());
        run_as_self.run_as_identity_id = Some("alice".to_string());
        let mut billed_elsewhere = own(agent_turn());
        billed_elsewhere.bill_to_identity_id = Some("system:indexing".to_string());
        let mut saved_by_herself = own(agent_turn());
        saved_by_herself.edited_by = Some("alice".to_string());
        let cases = [
            ("the default toolset", own(agent_turn())),
            (
                "posting for itself",
                own(turn_with(|payload| {
                    if let JobPayload::AgentTurn {
                        agent_owns_delivery,
                        ..
                    } = payload
                    {
                        *agent_owns_delivery = true;
                    }
                })),
            ),
            (
                "a stored team channel",
                own(turn_with(|payload| {
                    if let JobPayload::AgentTurn { channel_id, .. } = payload {
                        *channel_id = Some("C1".to_string());
                    }
                })),
            ),
            (
                "tools that post",
                own(turn_with(|payload| {
                    if let JobPayload::AgentTurn { tools, .. } = payload {
                        *tools = Some(vec!["messaging.send".to_string()]);
                    }
                })),
            ),
            ("an explicit run-as of herself", run_as_self),
            (
                "shared with everyone",
                job_fixture(Some("alice"), Some(VISIBILITY_GLOBAL), agent_turn()),
            ),
            ("billed to a system lane", billed_elsewhere),
            ("saved again by herself", saved_by_herself),
        ];
        for (why, job) in &cases {
            assert_eq!(
                run_author(job, std::slice::from_ref(job)),
                Principal,
                "{why}"
            );
        }
    }

    #[test]
    fn a_job_anyone_else_is_in_charge_of_is_not_the_persons_own() {
        use crate::serve::RunAuthor::Other;
        let mut on_behalf = job_fixture(Some("admin"), None, agent_turn());
        on_behalf.run_as_identity_id = Some("alice".to_string());
        let mut edited = job_fixture(Some("alice"), None, agent_turn());
        edited.edited_by = Some("admin".to_string());
        let mut automated = job_fixture(Some("alice"), None, agent_turn());
        automated.edited_by = Some("monitor".to_string());
        let mut system = job_fixture(Some("system:consolidation"), None, agent_turn());
        system.run_as_identity_id = Some("alice".to_string());
        let cases = [
            ("nobody created it", job_fixture(None, None, agent_turn())),
            ("an admin made it to run as her", on_behalf),
            ("an admin last edited it", edited),
            ("a monitor's turn created it", automated),
            ("the system's job", system),
        ];
        for (why, job) in &cases {
            assert_eq!(run_author(job, std::slice::from_ref(job)), Other, "{why}");
        }
    }

    #[test]
    fn an_old_stored_job_with_no_editor_record_is_judged_by_its_creator() {
        let mut stored =
            serde_json::to_value(job_fixture(Some("alice"), None, agent_turn())).unwrap();
        stored.as_object_mut().unwrap().remove("edited_by");
        let job: ScheduledJob = serde_json::from_value(stored).unwrap();
        assert_eq!(job.edited_by, None);
        assert_eq!(
            run_author(&job, std::slice::from_ref(&job)),
            crate::serve::RunAuthor::Principal
        );
    }

    #[test]
    fn a_pinned_job_is_its_persons_own_while_only_their_own_jobs_continue_its_session() {
        use crate::serve::RunAuthor::{Other, Principal};
        let pinned = |creator: &str, channel: &str| {
            let mut payload = agent_turn();
            if let JobPayload::AgentTurn {
                channel: pinned, ..
            } = &mut payload
            {
                *pinned = Some(channel.to_string());
            }
            let mut job = job_fixture(Some(creator), None, payload);
            job.run_as_identity_id = Some("alice".to_string());
            job
        };
        let own = pinned("alice", "ios");
        let also_own = pinned("alice", "ios");
        let on_behalf = pinned("admin", "ios");
        let elsewhere = pinned("admin", "slack:w1:D1:17.1");
        assert_eq!(
            run_author(&own, &[own.clone(), also_own, elsewhere.clone()]),
            Principal
        );
        assert_eq!(
            run_author(&own, &[own.clone(), on_behalf.clone(), elsewhere]),
            Other,
            "an admin's job run as her writes to the session her job replays"
        );
        assert_eq!(run_author(&on_behalf, &[own, on_behalf.clone()]), Other);
    }

    #[tokio::test]
    async fn a_schedule_edit_records_its_editor_and_the_creators_save_makes_it_theirs_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (service, _server) = crate::serve::testing::service_on_mock_llm(tmp.path()).await;
        let sched = Scheduler::new(service);
        let id = sched
            .add_job_owned(
                "own",
                None,
                Schedule::Every {
                    every_secs: 86_400,
                    anchor: None,
                },
                agent_turn(),
                Some("alice".to_string()),
                None,
            )
            .await
            .unwrap();
        let editor = |job: Option<ScheduledJob>| job.unwrap().edited_by;

        let paused = sched
            .update_job(
                &id,
                JobEdit {
                    name: Some("renamed".into()),
                    enabled: Some(false),
                    ..Default::default()
                },
                "admin",
            )
            .await
            .unwrap();
        assert_eq!(
            editor(paused),
            None,
            "renaming or pausing changes nothing it does"
        );
        assert!(
            sched
                .update_job_bill_to(&id, Some("system:indexing".into()))
                .await
        );
        assert_eq!(
            editor(sched.get_job(&id).await),
            None,
            "billing is not control"
        );

        let hourly = || Schedule::Every {
            every_secs: 3_600,
            anchor: None,
        };
        let retime = || JobEdit {
            schedule: Some(hourly()),
            ..Default::default()
        };
        let retimed = sched.update_job(&id, retime(), "admin").await.unwrap();
        assert_eq!(editor(retimed), Some("admin".to_string()));

        let resave = |by: &'static str| {
            let sched = &sched;
            let id = id.clone();
            async move {
                sched
                    .update_job(&id, JobEdit::default(), by)
                    .await
                    .unwrap()
                    .unwrap()
            }
        };
        assert!(is_own(&resave("alice").await));

        for (what, edit) in [
            ("its session", edit_session(SessionMode::Pinned)),
            (
                "its tools",
                JobEdit {
                    tools: Some(vec!["notes".into()]),
                    ..Default::default()
                },
            ),
            (
                "who may read it",
                JobEdit {
                    visibility: Some(VISIBILITY_GLOBAL.into()),
                    ..Default::default()
                },
            ),
        ] {
            let changed = sched.update_job(&id, edit, "admin").await.unwrap();
            assert_eq!(editor(changed), Some("admin".to_string()), "{what}");
            assert!(is_own(&resave("alice").await), "{what}");
        }

        let shared = sched
            .update_job(
                &id,
                JobEdit {
                    visibility: Some(VISIBILITY_GLOBAL.into()),
                    ..Default::default()
                },
                "admin",
            )
            .await
            .unwrap();
        assert_eq!(
            editor(shared),
            Some("admin".to_string()),
            "who may read its history is hers to choose"
        );

        let resaved_by_a_run = workforce_agent::runner::CURRENT_SESSION_KEY
            .scope(
                "scheduler:job:1700000000000".to_string(),
                sched.update_job(&id, retime(), "alice"),
            )
            .await
            .unwrap();
        assert_eq!(
            editor(resaved_by_a_run),
            Some("scheduler".to_string()),
            "a run as her is not her saving it"
        );
    }

    #[tokio::test]
    async fn a_job_an_automated_turn_creates_records_that_source() {
        let tmp = tempfile::tempdir().unwrap();
        let (service, _server) = crate::serve::testing::service_on_mock_llm(tmp.path()).await;
        let sched = Scheduler::new(service);
        let daily = || Schedule::Every {
            every_secs: 86_400,
            anchor: None,
        };
        for (session, origin) in [
            ("monitor:m1:1.1", Some("monitor")),
            ("heartbeat:inbox:1700000000000", Some("heartbeat")),
            ("scheduler:job:1700000000000", Some("scheduler")),
            ("workflow:run:step", Some("workflow")),
            ("slack:w1:C1:17.1", None),
            ("ios", None),
            ("agent-job:j1", None),
        ] {
            let id = workforce_agent::runner::CURRENT_SESSION_KEY
                .scope(
                    session.to_string(),
                    workforce_agent::runner::CURRENT_USER_ID.scope(
                        "alice".to_string(),
                        sched.add_job(session, daily(), agent_turn()),
                    ),
                )
                .await
                .unwrap();
            let job = sched.get_job(&id).await.unwrap();
            assert_eq!(
                job.creator_identity_id.as_deref(),
                Some("alice"),
                "{session}"
            );
            assert_eq!(job.edited_by.as_deref(), origin, "{session}");
        }
    }

    /// A sub-agent runs in a session of its own, so the automation that
    /// spawned it travels with the job that runs it, into the tool task a
    /// job is created in; one a person's own turn spawned carries none.
    #[tokio::test]
    async fn a_job_a_sub_agent_creates_records_the_automation_that_spawned_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (service, _server) = crate::serve::testing::service_on_mock_llm(tmp.path()).await;
        let sched = Arc::new(Scheduler::new(service));
        for (spawned_in, origin) in [
            ("heartbeat:inbox:1700000000000", Some("heartbeat")),
            ("monitor:m1:1.1", Some("monitor")),
            ("slack:w1:D1:17.1", None),
        ] {
            let submitted = workforce_agent::runner::CURRENT_SESSION_KEY.sync_scope(
                spawned_in.to_string(),
                || {
                    crate::job_runner::AgentJobConfig::new(
                        "follow up".to_string(),
                        crate::job_runner::AgentProfile::Full,
                        workforce_common::session::TurnPrincipal::run_as(Some("alice".into())),
                    )
                },
            );
            assert_eq!(
                submitted.automated_origin.as_deref(),
                origin,
                "{spawned_in}"
            );

            let sched = Arc::clone(&sched);
            let created = descending_from(
                submitted.automated_origin,
                workforce_agent::runner::CURRENT_SESSION_KEY.scope(
                    "agent-job:j1".to_string(),
                    workforce_agent::runner::CURRENT_USER_ID.scope(
                        "alice".to_string(),
                        // Spawned as the runner spawns a tool call.
                        async {
                            workforce_agent::runner::spawn_in_session_context(async move {
                                let id = sched
                                    .add_job(
                                        "follow-up",
                                        Schedule::Every {
                                            every_secs: 86_400,
                                            anchor: None,
                                        },
                                        agent_turn(),
                                    )
                                    .await
                                    .unwrap();
                                sched.get_job(&id).await.unwrap()
                            })
                            .await
                        },
                    ),
                ),
            )
            .await
            .unwrap();
            assert_eq!(created.edited_by.as_deref(), origin, "{spawned_in}");
            assert_eq!(is_own(&created), origin.is_none(), "{spawned_in}");
        }
    }

    #[test]
    fn only_a_persons_job_session_continuing_a_web_conversation_reads_as_one() {
        assert!(continues_conversation(&continued_session_channel(
            "ios",
            Some("alice")
        )));
        for session in [
            continued_session_channel("slack:w1:D1:17.1", Some("alice")),
            continued_session_channel("ios", None),
            isolated_run_channel("scheduler", "job", Utc::now()),
        ] {
            assert!(!continues_conversation(&session), "{session}");
        }
    }

    /// Two people's jobs continuing a session of the same name — every web
    /// conversation's is `ios` — each replay their own runs and never the
    /// other's.
    #[tokio::test]
    async fn a_job_continuing_a_session_never_replays_another_persons_runs() {
        const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
        const BOB: &str = "0b7f3d52-8f0a-4c2f-9d44-3c1e6a1f4e22";
        let tmp = tempfile::tempdir().unwrap();
        let (service, server) = crate::serve::testing::service_on_mock_llm(tmp.path()).await;
        let sched = Scheduler::new(service);
        let mut jobs = Vec::new();
        for (person, message) in [(ALICE, "ALICE-JOB-MARKER"), (BOB, "BOB-JOB-MARKER")] {
            let payload = JobPayload::AgentTurn {
                message: message.to_string(),
                channel: Some("ios".to_string()),
                channel_id: None,
                thread_id: None,
                model: None,
                user_id: None,
                max_rounds: None,
                agent_owns_delivery: false,
                tools: None,
            };
            let every = Schedule::Every {
                every_secs: 86_400,
                anchor: None,
            };
            let id = workforce_agent::runner::CURRENT_USER_ID
                .scope(person.to_string(), sched.add_job(message, every, payload))
                .await
                .unwrap();
            jobs.push(id);
        }
        for id in jobs.iter().chain(&jobs) {
            assert!(sched.run_job_now(id).await.unwrap().success);
        }

        let sent: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .map(|request| String::from_utf8_lossy(&request.body).into_owned())
            .collect();
        let of = |marker: &str| -> Vec<&String> {
            sent.iter().filter(|body| body.contains(marker)).collect()
        };
        assert!(
            of("ALICE-JOB-MARKER").len() >= 2 && of("BOB-JOB-MARKER").len() >= 2,
            "each job ran twice"
        );
        assert!(
            of("ALICE-JOB-MARKER")
                .last()
                .is_some_and(|body| body.matches("ALICE-JOB-MARKER").count() >= 2),
            "a job's second run continues its first"
        );
        assert!(
            !sent
                .iter()
                .any(|body| body.contains("ALICE-JOB-MARKER") && body.contains("BOB-JOB-MARKER")),
            "one person's run replayed the other's"
        );
    }

    #[test]
    fn a_continued_session_is_one_per_person_a_job_runs_as() {
        assert_eq!(continued_session_channel("ios", None), "ios");
        assert_eq!(
            continued_session_channel("slack:w1:C1:17.1", Some("alice")),
            "scheduler:alice:slack:w1:C1:17.1"
        );
        assert!(crate::turn_extraction::is_internal_channel(
            &continued_session_channel("ios", Some("alice"))
        ));
        assert_ne!(
            continued_session_channel("ios", Some("alice")),
            continued_session_channel("ios", Some("bob"))
        );
    }

    #[test]
    fn effective_run_as_identity_prefers_run_as_then_creator() {
        let creator_only = job_fixture(Some("creator"), None, agent_turn());
        assert_eq!(creator_only.effective_run_as_identity(), Some("creator"));

        let mut overridden = job_fixture(Some("creator"), None, agent_turn());
        overridden.run_as_identity_id = Some("runner".to_string());
        assert_eq!(overridden.effective_run_as_identity(), Some("runner"));

        assert_eq!(
            job_fixture(None, None, agent_turn()).effective_run_as_identity(),
            None
        );
        assert_eq!(
            job_fixture(Some(""), None, agent_turn()).effective_run_as_identity(),
            None
        );
    }

    #[test]
    fn effective_billing_identity_prefers_bill_to_then_run_as() {
        let mut job = job_fixture(Some("creator"), None, agent_turn());
        assert_eq!(job.effective_billing_identity(), Some("creator"));

        job.run_as_identity_id = Some("runner".to_string());
        assert_eq!(job.effective_billing_identity(), Some("runner"));

        job.bill_to_identity_id = Some("system:indexing".to_string());
        assert_eq!(job.effective_billing_identity(), Some("system:indexing"));
        assert_eq!(
            job.effective_run_as_identity(),
            Some("runner"),
            "billing never changes who the job runs as"
        );

        job.bill_to_identity_id = Some(String::new());
        assert_eq!(job.effective_billing_identity(), Some("runner"));
    }

    #[test]
    fn jobs_persisted_before_bill_to_deserialize_cleanly() {
        let mut legacy = serde_json::to_value(job_fixture(Some("c"), None, agent_turn())).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("bill_to_identity_id");
        let job: ScheduledJob = serde_json::from_value(legacy).unwrap();
        assert_eq!(job.bill_to_identity_id, None);
    }

    #[test]
    fn ownerless_job_is_global_by_default() {
        let job = job_fixture(None, None, agent_turn());
        assert_eq!(job.owner(), None);
        assert_eq!(job.effective_visibility(), VISIBILITY_GLOBAL);
    }

    #[test]
    fn owned_job_is_private_by_default() {
        let job = job_fixture(Some("user-1"), None, agent_turn());
        assert_eq!(job.owner(), Some("user-1"));
        assert_eq!(job.effective_visibility(), VISIBILITY_PRIVATE);
    }

    #[test]
    fn explicit_visibility_overrides_default() {
        let job = job_fixture(Some("user-1"), Some(VISIBILITY_GLOBAL), agent_turn());
        assert_eq!(job.effective_visibility(), VISIBILITY_GLOBAL);
        let job = job_fixture(None, Some(VISIBILITY_PRIVATE), agent_turn());
        assert_eq!(job.effective_visibility(), VISIBILITY_PRIVATE);
    }

    #[test]
    fn reminder_owner_falls_back_to_recipient() {
        let payload = JobPayload::Reminder {
            text: "ping".to_string(),
            user_id: "user-2".to_string(),
        };
        let job = job_fixture(None, None, payload);
        assert_eq!(job.owner(), Some("user-2"));
        assert_eq!(job.effective_visibility(), VISIBILITY_PRIVATE);
    }

    #[test]
    fn reminder_is_always_private_even_if_marked_global() {
        let payload = JobPayload::Reminder {
            text: "ping".to_string(),
            user_id: "user-2".to_string(),
        };
        let job = job_fixture(Some("user-2"), Some(VISIBILITY_GLOBAL), payload);
        assert_eq!(job.effective_visibility(), VISIBILITY_PRIVATE);
    }

    // ── owner-scope predicates (mirror the gateway exactly) ──────────────────

    #[test]
    fn owned_job_visible_and_mutable_only_by_owner_or_admin() {
        let job = job_fixture(Some("user-A"), None, agent_turn());
        // Owner: sees and mutates.
        assert!(schedule_visible_to(&job, Some("user-A"), false));
        assert!(schedule_mutable_by(&job, Some("user-A"), false));
        // Other user: neither.
        assert!(!schedule_visible_to(&job, Some("user-B"), false));
        assert!(!schedule_mutable_by(&job, Some("user-B"), false));
        // Admin: both, regardless of identity.
        assert!(schedule_visible_to(&job, Some("user-B"), true));
        assert!(schedule_mutable_by(&job, Some("user-B"), true));
        assert!(schedule_visible_to(&job, None, true));
        assert!(schedule_mutable_by(&job, None, true));
    }

    #[test]
    fn global_non_reminder_job_visible_to_all_but_mutable_only_by_owner() {
        let job = job_fixture(Some("user-A"), Some(VISIBILITY_GLOBAL), agent_turn());
        // Non-owner: can SEE a global job but cannot MUTATE it.
        assert!(schedule_visible_to(&job, Some("user-B"), false));
        assert!(!schedule_mutable_by(&job, Some("user-B"), false));
        // Owner: both.
        assert!(schedule_visible_to(&job, Some("user-A"), false));
        assert!(schedule_mutable_by(&job, Some("user-A"), false));
    }

    #[test]
    fn reminders_never_visible_to_non_owner_even_if_global_marked() {
        let payload = JobPayload::Reminder {
            text: "ping".to_string(),
            user_id: "user-A".to_string(),
        };
        // Marked global, but reminders are forced private.
        let job = job_fixture(Some("user-A"), Some(VISIBILITY_GLOBAL), payload);
        assert!(!schedule_visible_to(&job, Some("user-B"), false));
        assert!(!schedule_mutable_by(&job, Some("user-B"), false));
        assert!(schedule_visible_to(&job, Some("user-A"), false));
        assert!(schedule_mutable_by(&job, Some("user-A"), false));
    }

    #[test]
    fn empty_caller_id_never_matches_empty_owner() {
        // Fail-closed: an empty caller must not match an empty/absent owner.
        let ownerless_private = job_fixture(None, Some(VISIBILITY_PRIVATE), agent_turn());
        assert!(!schedule_visible_to(&ownerless_private, Some(""), false));
        assert!(!schedule_mutable_by(&ownerless_private, Some(""), false));
        assert!(!schedule_visible_to(&ownerless_private, None, false));
        assert!(!schedule_mutable_by(&ownerless_private, None, false));

        // An empty-string owner is also normalised to "no owner" by `owner()`.
        let empty_owner = job_fixture(Some(""), None, agent_turn());
        assert_eq!(empty_owner.owner(), None);
        assert!(!schedule_mutable_by(&empty_owner, Some(""), false));
    }

    #[test]
    fn ownerless_global_job_visible_to_anyone_but_mutable_only_by_admin() {
        // System/legacy jobs (no owner) default to global visibility.
        let job = job_fixture(None, None, agent_turn());
        assert_eq!(job.effective_visibility(), VISIBILITY_GLOBAL);
        assert!(schedule_visible_to(&job, Some("user-A"), false));
        // No owner ⇒ no non-admin can mutate.
        assert!(!schedule_mutable_by(&job, Some("user-A"), false));
        assert!(schedule_mutable_by(&job, None, true));
    }

    #[test]
    fn legacy_json_without_visibility_deserializes() {
        let json = serde_json::json!({
            "id": "j1",
            "name": "legacy",
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "payload": { "kind": "monitor_sweep" },
            "enabled": true,
            "created_at": "2026-01-01T00:00:00Z",
            "run_count": 0,
        });
        let job: ScheduledJob = serde_json::from_value(json).unwrap();
        assert_eq!(job.visibility, None);
        assert_eq!(job.effective_visibility(), VISIBILITY_GLOBAL);
    }

    // `load_jobs` parses the whole `Vec<ScheduledJob>` at once and loads nothing
    // if any entry fails, so a populated pre-upgrade `jobs.json` (no `visibility`
    // field, every payload variant) must still deserialize wholesale.
    #[test]
    fn legacy_jobs_array_loads_every_variant() {
        let json = serde_json::json!([
            {
                "id": "a", "name": "daily-report", "description": "Send report",
                "schedule": { "kind": "cron", "expr": "0 9 * * *", "tz": "Europe/London" },
                "payload": { "kind": "agent_turn", "message": "go", "channel_id": "C1" },
                "enabled": true, "created_at": "2026-01-01T00:00:00Z",
                "last_run": "2026-06-01T09:00:00Z", "next_run": "2026-06-02T09:00:00Z",
                "run_count": 42, "creator_identity_id": "user-1"
            },
            {
                "id": "b", "name": "ping",
                "schedule": { "kind": "at", "at": "2026-12-01T10:00:00Z" },
                "payload": { "kind": "reminder", "text": "stand-up", "user_id": "user-2" },
                "enabled": true, "created_at": "2026-05-01T00:00:00Z", "run_count": 0,
                "is_reminder": true, "delivery_failures": 1, "creator_identity_id": "user-2"
            },
            {
                "id": "c", "name": "monitor-sweep",
                "schedule": { "kind": "every", "every_secs": 300 },
                "payload": { "kind": "monitor_sweep" },
                "enabled": true, "created_at": "2026-03-01T00:00:00Z", "run_count": 1000
            },
            {
                "id": "d", "name": "kickoff",
                "schedule": { "kind": "cron", "expr": "*/15 * * * *" },
                "payload": { "kind": "workflow_start", "workflow_id": "wf-1" },
                "enabled": false, "created_at": "2026-04-01T00:00:00Z", "run_count": 7,
                "run_as_identity_id": "user-9"
            },
            {
                "id": "e", "name": "nudge",
                "schedule": { "kind": "at", "at": "2026-07-01T10:00:00Z" },
                "payload": { "kind": "system_event", "text": "wake", "channel_id": "C2" },
                "enabled": true, "created_at": "2026-02-01T00:00:00Z", "run_count": 3
            }
        ]);
        let jobs: Vec<ScheduledJob> = serde_json::from_value(json).unwrap();
        assert_eq!(jobs.len(), 5);
        assert!(jobs.iter().all(|j| j.visibility.is_none()));
        // Owned jobs become private; ownerless system jobs stay globally visible.
        assert_eq!(jobs[0].effective_visibility(), VISIBILITY_PRIVATE);
        assert_eq!(jobs[1].owner(), Some("user-2"));
        assert_eq!(jobs[1].effective_visibility(), VISIBILITY_PRIVATE);
        assert_eq!(jobs[2].effective_visibility(), VISIBILITY_GLOBAL);
        assert_eq!(jobs[3].effective_visibility(), VISIBILITY_GLOBAL);
        assert_eq!(jobs[4].effective_visibility(), VISIBILITY_GLOBAL);
        assert_eq!(jobs[0].run_count, 42);
        assert!(!jobs[3].enabled);
    }

    #[test]
    fn a_run_record_written_before_notes_existed_still_loads() {
        let stored = r#"{"job_id":"j","job_name":"n","started_at":"2026-01-01T00:00:00Z",
            "finished_at":"2026-01-01T00:00:01Z","duration_ms":1000,"success":true,
            "output":"ok","delivered":true}"#;
        let record: JobRunRecord = serde_json::from_str(stored).unwrap();
        assert_eq!(record.note, None);
        assert!(!serde_json::to_string(&record).unwrap().contains("note"));
    }

    #[test]
    fn run_record_serialization() {
        let record = JobRunRecord {
            job_id: "abc".to_string(),
            job_name: "test".to_string(),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            duration_ms: 1500,
            success: true,
            output: "All clear".to_string(),
            delivered: true,
            note: None,
        };

        let json = serde_json::to_string(&record).unwrap();
        let deserialized: JobRunRecord = serde_json::from_str(&json).unwrap();
        assert!(deserialized.success);
        assert_eq!(deserialized.duration_ms, 1500);
    }

    #[test]
    fn workflow_start_payload_round_trip() {
        let payload = JobPayload::WorkflowStart {
            workflow_id: "daily-digest".to_string(),
            payload: serde_json::json!({"scope": "all"}),
            channel_id: Some("C123".to_string()),
            thread_id: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"kind\":\"workflow_start\""));
        let back: JobPayload = serde_json::from_str(&json).unwrap();
        match back {
            JobPayload::WorkflowStart {
                workflow_id,
                payload,
                channel_id,
                thread_id,
            } => {
                assert_eq!(workflow_id, "daily-digest");
                assert_eq!(payload, serde_json::json!({"scope": "all"}));
                assert_eq!(channel_id.as_deref(), Some("C123"));
                assert!(thread_id.is_none());
            }
            _ => panic!("expected WorkflowStart variant"),
        }
    }

    #[test]
    fn workflow_start_payload_defaults() {
        // Missing optional fields should deserialize to defaults.
        let json = r#"{"kind":"workflow_start","workflow_id":"x"}"#;
        let payload: JobPayload = serde_json::from_str(json).unwrap();
        match payload {
            JobPayload::WorkflowStart {
                workflow_id,
                payload,
                channel_id,
                thread_id,
            } => {
                assert_eq!(workflow_id, "x");
                assert!(payload.is_null());
                assert!(channel_id.is_none());
                assert!(thread_id.is_none());
            }
            _ => panic!("expected WorkflowStart variant"),
        }
    }

    use chrono::Timelike;

    // ── voice delivery ───────────────────────────────────────────────────

    /// Create a job with the task-locals a live turn carries, the way the
    /// scheduler/reminder tools do from inside the runner. `voice_channel` is
    /// what the gateway resolved for THIS turn — empty for every surface but
    /// a voice bridge.
    async fn add_job_in_turn(
        sched: &Arc<Scheduler>,
        voice_channel: &str,
        payload: JobPayload,
    ) -> String {
        workforce_agent::runner::CURRENT_VOICE_CHANNEL
            .scope(
                voice_channel.to_string(),
                workforce_agent::runner::CURRENT_USER_ID.scope(
                    "user-A".to_string(),
                    sched.add_job_with_desc(
                        "spoken",
                        None,
                        Schedule::At {
                            at: Utc::now() + chrono::Duration::seconds(30),
                        },
                        payload,
                    ),
                ),
            )
            .await
            .unwrap()
    }

    fn reminder_payload() -> JobPayload {
        JobPayload::Reminder {
            text: "stretch".into(),
            user_id: "user-A".into(),
        }
    }

    #[tokio::test]
    async fn a_job_created_from_a_spoken_bridge_turn_remembers_the_voice_target() {
        let sched = test_scheduler().await;
        let id = add_job_in_turn(&sched, "voice-pe", reminder_payload()).await;
        let job = sched.get_job(&id).await.unwrap();
        assert_eq!(
            job.deliver_to,
            Some(DeliveryTarget {
                surface: DELIVERY_SURFACE_VOICE.to_string(),
                channel: "voice-pe".to_string(),
            })
        );
    }

    /// Everything that is not a spoken bridge turn — typed gateway, Slack,
    /// workflow steps, iOS (a screen the reply already reaches) — keeps
    /// today's Slack delivery. The gateway resolves this per turn, so a web
    /// turn running alongside a spoken one cannot inherit its target.
    #[tokio::test]
    async fn every_other_surface_keeps_slack_delivery() {
        let sched = test_scheduler().await;
        let id = add_job_in_turn(&sched, "", reminder_payload()).await;
        assert!(sched.get_job(&id).await.unwrap().deliver_to.is_none());
    }

    /// A job created outside any turn (REST, system registration) carries no
    /// task-locals at all and must not fall over reading them.
    #[tokio::test]
    async fn a_job_created_outside_a_turn_has_no_target() {
        let sched = test_scheduler().await;
        let id = sched
            .add_job_with_desc(
                "system",
                None,
                Schedule::At {
                    at: Utc::now() + chrono::Duration::seconds(30),
                },
                reminder_payload(),
            )
            .await
            .unwrap();
        assert!(sched.get_job(&id).await.unwrap().deliver_to.is_none());
    }

    #[tokio::test]
    async fn a_voice_targeted_reminder_is_announced_and_counts_as_delivered() {
        let sched = test_scheduler().await;
        let listeners = Arc::new(crate::voice_announce::VoiceListeners::default());
        let _bridge = listeners.register("voice-pe", Some("user-A".into()));
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        sched.set_voice_announcer(Arc::new(crate::voice_announce::VoiceAnnouncer::new(
            Arc::clone(&listeners),
            tx,
            None,
        )));

        let id = add_job_in_turn(&sched, "voice-pe", reminder_payload()).await;
        let record = sched.run_job_now(&id).await.unwrap();

        assert!(record.delivered, "an announced reminder is delivered");
        let msg = rx.try_recv().expect("the announcement should be queued");
        assert_eq!(msg.msg_type, "reminder");
        assert_eq!(msg.content, "stretch");
        assert_eq!(msg.target_channel.as_deref(), Some("voice-pe"));
        assert_eq!(
            msg.target_identity.as_deref(),
            Some("user-A"),
            "announcements are scoped to the job's owner"
        );
    }

    /// The silent-no-op this feature can fail into: a bridge IS connected on
    /// the right channel, but its device never linked to an identity, so the
    /// announcement is refused and delivery falls back to Slack. Symptomless
    /// from the user's side (the reminder still arrives, just in the wrong
    /// place), so it is pinned here rather than left to a live smoke test.
    #[tokio::test]
    async fn an_unlinked_bridge_does_not_receive_and_falls_back() {
        let sched = test_scheduler().await;
        let listeners = Arc::new(crate::voice_announce::VoiceListeners::default());
        // Registered, right channel, but no identity — a paired device that
        // was never linked to its owner.
        let _bridge = listeners.register("voice-pe", None);
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        sched.set_voice_announcer(Arc::new(crate::voice_announce::VoiceAnnouncer::new(
            Arc::clone(&listeners),
            tx,
            None,
        )));

        let id = add_job_in_turn(&sched, "voice-pe", reminder_payload()).await;
        let record = sched.run_job_now(&id).await.unwrap();

        assert!(
            rx.try_recv().is_err(),
            "an unlinked device must not be told another identity's reminder"
        );
        assert!(
            !record.delivered,
            "refused voice delivery must fall through to Slack, not count as delivered"
        );
    }

    /// No bridge connected and (in tests) no Slack either: the run counts as
    /// undelivered so the existing retry-with-backoff machinery takes over
    /// and announces once the bridge returns.
    #[tokio::test]
    async fn a_voice_reminder_with_no_bridge_falls_back_and_is_undelivered() {
        let sched = test_scheduler().await;
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        sched.set_voice_announcer(Arc::new(crate::voice_announce::VoiceAnnouncer::new(
            Arc::new(crate::voice_announce::VoiceListeners::default()),
            tx,
            None,
        )));

        let id = add_job_in_turn(&sched, "voice-pe", reminder_payload()).await;
        let record = sched.run_job_now(&id).await.unwrap();

        assert!(!record.delivered, "nothing was listening anywhere");
        assert!(rx.try_recv().is_err(), "no announcement should be queued");
    }

    /// The new field must not break pre-upgrade rows — `load_jobs` drops the
    /// whole persisted file on any parse failure — and must round-trip.
    #[test]
    fn deliver_to_defaults_on_legacy_rows_and_round_trips() {
        let legacy = serde_json::json!({
            "id": "legacy-2",
            "name": "legacy",
            "schedule": { "kind": "every", "every_secs": 300 },
            "payload": { "kind": "agent_turn", "message": "hi" },
            "enabled": true,
            "created_at": "2026-08-13T15:00:18Z",
            "last_run": null,
            "next_run": null,
            "run_count": 1
        });
        let job: ScheduledJob = serde_json::from_value(legacy).unwrap();
        assert!(job.deliver_to.is_none());

        let mut job = job;
        job.deliver_to = Some(DeliveryTarget {
            surface: DELIVERY_SURFACE_VOICE.to_string(),
            channel: "voice-pe".to_string(),
        });
        let back: ScheduledJob =
            serde_json::from_value(serde_json::to_value(&job).unwrap()).unwrap();
        assert_eq!(back.deliver_to, job.deliver_to);
    }

    #[tokio::test]
    async fn isolated_run_channel_matches_scheduler_task_id_shape() {
        let sched = test_scheduler().await;
        let job = job_fixture(None, None, agent_turn());
        let at = Utc::now();
        let channel = isolated_run_channel("scheduler", &job.id, at);
        assert_eq!(
            channel,
            format!("scheduler:{}:{}", job.id, at.timestamp_millis())
        );
        assert_ne!(
            channel,
            isolated_run_channel("scheduler", &job.id, at + chrono::Duration::milliseconds(1)),
            "a racing manual run gets its own session"
        );

        let budgeted = ScheduledJob {
            budget_usd: Some(1.0),
            ..job
        };
        sched
            .service()
            .telemetry_store
            .record_cost(&workforce_common::persistence::CostRecord {
                agent_id: "a".to_string(),
                model: "m".to_string(),
                cost_usd: 2.0,
                task_id: Some(channel),
                ..Default::default()
            })
            .unwrap();
        assert!(
            sched
                .cap_breach(&budgeted, &Utc::now())
                .is_some_and(|r| r.contains("budget")),
            "the run's spend is found by the job's task prefix"
        );
    }

    #[test]
    fn agent_turn_payload_without_tools_deserialises_legacy_row() {
        let legacy = serde_json::json!({
            "kind": "agent_turn",
            "message": "go",
            "agent_owns_delivery": false
        });
        let payload: JobPayload = serde_json::from_value(legacy).unwrap();
        assert!(matches!(payload, JobPayload::AgentTurn { tools: None, .. }));
        assert!(
            serde_json::to_value(&payload)
                .unwrap()
                .get("tools")
                .is_none(),
            "an unscoped job writes its row as before"
        );
    }

    #[test]
    fn agent_turn_tools_round_trip() {
        let scoped = JobPayload::AgentTurn {
            message: "triage".to_string(),
            channel: None,
            channel_id: None,
            thread_id: None,
            model: None,
            user_id: None,
            max_rounds: None,
            agent_owns_delivery: false,
            tools: Some(vec!["github".to_string(), "messaging.send".to_string()]),
        };
        let back: JobPayload =
            serde_json::from_value(serde_json::to_value(&scoped).unwrap()).unwrap();
        assert!(matches!(
            back,
            JobPayload::AgentTurn { tools: Some(ref t), .. } if t == &["github", "messaging.send"]
        ));
    }
}
