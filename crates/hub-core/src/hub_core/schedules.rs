//! The shapes of scheduled jobs: schedules, payloads, edits, delivery and run records.

use chrono::DateTime;
use chrono::Utc;
use chrono_tz::Tz;
use serde::Deserialize;
use serde::Serialize;
use tracing::warn;
use uuid::Uuid;

/// [`ScheduledJob::edited_by`] for a save made with the hub's owner token,
/// which names nobody.
pub const OWNER_TOKEN_EDITOR: &str = "rest:owner-token";

/// A scheduled job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJob {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub schedule: Schedule,
    pub payload: JobPayload,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub last_run: Option<DateTime<Utc>>,
    pub next_run: Option<DateTime<Utc>>,
    pub run_count: u64,
    /// Whether this job is a simple reminder (direct DM delivery, no agent).
    #[serde(default)]
    pub is_reminder: bool,
    /// Number of consecutive delivery failures (reset on success).
    #[serde(default)]
    pub delivery_failures: u32,
    /// Identity UUID of the user who created this job.
    /// Used for credential resolution (per-user vault) when the job fires.
    #[serde(default)]
    pub creator_identity_id: Option<String>,
    /// Override: use this identity's credentials instead of the creator's.
    /// Set by admins to control which user's config a scheduled task uses.
    #[serde(default)]
    pub run_as_identity_id: Option<String>,
    /// Identity CHARGED for this job's LLM spend, when it should differ from
    /// the identity the job runs as — so background automation that needs a
    /// human's access doesn't drain that human's personal budget allowance.
    /// SysAdmin-set (typically a `system:*` lane); absent = bill the run-as
    /// identity as before.
    #[serde(default)]
    pub bill_to_identity_id: Option<String>,
    /// `"private"` (owner only) or `"global"` (anyone may see/run). Absent on
    /// legacy rows; resolved by [`ScheduledJob::effective_visibility`].
    #[serde(default)]
    pub visibility: Option<String>,
    /// Auto-disable after this many runs. `None` = unlimited (system jobs,
    /// legacy rows). Defaulted from `[budget] job_max_runs` on owned
    /// recurring agent jobs.
    #[serde(default)]
    pub max_runs: Option<u64>,
    /// Auto-disable once the job's cumulative LLM spend reaches this.
    /// `None` = unlimited. Defaulted from `[budget] job_total_usd`.
    #[serde(default)]
    pub budget_usd: Option<f64>,
    /// Auto-disable after this instant. `None` = never. Defaulted to
    /// `created_at + [budget] job_expiry_days` on owned recurring agent jobs.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// Where the result should be delivered instead of the default Slack DM.
    /// Stamped at creation from the originating turn (currently only voice
    /// bridges); delivery falls back to Slack when the target is unreachable.
    #[serde(default)]
    pub deliver_to: Option<DeliveryTarget>,
    /// Who last changed when the job runs or who may read it, when that was
    /// not its creator: the identity that saved its schedule or visibility,
    /// or the automated source (`heartbeat`, `monitor`, `scheduler`, …) whose
    /// turn created or saved it.
    /// Absent on jobs only their creator has touched, legacy rows included.
    #[serde(default)]
    pub edited_by: Option<String>,
    /// Switched off because the person it runs for was suspended, and to be
    /// switched back on when they are not. `enabled` carries the pause, so a
    /// hub build that predates this field leaves the job off too; an explicit
    /// pause or resume by anyone supersedes it. Always written, so a shared
    /// store's field merge clears it as well as sets it.
    #[serde(default)]
    pub paused_by_suspension: bool,
}

/// A non-Slack delivery destination remembered on a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryTarget {
    /// Which kind of surface — only [`DELIVERY_SURFACE_VOICE`] is honoured.
    pub surface: String,
    /// Surface-specific channel: for voice, the bridge's gateway channel
    /// name (e.g. `voice-pe`).
    pub channel: String,
}

/// `DeliveryTarget::surface` for voice-bridge announcement delivery.
pub const DELIVERY_SURFACE_VOICE: &str = "voice";

impl DeliveryTarget {
    pub fn is_voice(&self) -> bool {
        self.surface == DELIVERY_SURFACE_VOICE
    }
}

/// Visibility: only the owner (and admins) may see, run, or edit the job.
pub const VISIBILITY_PRIVATE: &str = "private";

/// Visibility: any authenticated caller may see the job; running and editing
/// stay restricted to the owner and admins.
pub const VISIBILITY_GLOBAL: &str = "global";

impl ScheduledJob {
    /// Whether the job acts for, bills, or was last saved by anyone in
    /// `names`.
    pub fn names_any(&self, names: &[String]) -> bool {
        let named = |id: Option<&str>| id.is_some_and(|id| names.iter().any(|n| n == id));
        named(self.creator_identity_id.as_deref())
            || named(self.run_as_identity_id.as_deref())
            || named(self.bill_to_identity_id.as_deref())
            || named(self.edited_by.as_deref())
            || named(self.payload.user_id())
    }

    /// Whether the job is the property of anyone in `names`: they created
    /// it, or it is a legacy reminder of theirs that names no creator.
    pub fn owned_by(&self, names: &[String]) -> bool {
        self.owner()
            .is_some_and(|owner| names.iter().any(|n| n == owner))
    }

    /// Make every identity field naming anyone in `names` name `to`.
    /// Returns the top-level fields it changed.
    pub fn reassign(&mut self, names: &[String], to: &str) -> Vec<&'static str> {
        let named = |id: &Option<String>| {
            id.as_deref()
                .is_some_and(|id| names.iter().any(|n| n == id))
        };
        let mut fields = Vec::new();
        for (field, slot) in [
            ("creator_identity_id", &mut self.creator_identity_id),
            ("run_as_identity_id", &mut self.run_as_identity_id),
            ("bill_to_identity_id", &mut self.bill_to_identity_id),
            ("edited_by", &mut self.edited_by),
        ] {
            if named(slot) {
                *slot = Some(to.to_string());
                fields.push(field);
            }
        }
        if self.payload.reassign_user(names, to) {
            fields.push("payload");
        }
        fields
    }

    /// Identity UUID that owns this job, or `None` for system/operator jobs.
    /// Prefers the recorded creator; falls back to a reminder's recipient so
    /// legacy reminders written before `creator_identity_id` existed still
    /// resolve to their owner.
    pub fn owner(&self) -> Option<&str> {
        self.creator_identity_id
            .as_deref()
            .or(match &self.payload {
                JobPayload::Reminder { user_id, .. } => Some(user_id.as_str()),
                _ => None,
            })
            .filter(|s| !s.is_empty())
    }

    /// Identity the agent turn executes *as* — credential, memory, and
    /// channel-membership scoping key on it. Prefers an explicit `run_as`
    /// override, else the creator; `None` for system jobs, which run unscoped.
    pub fn effective_run_as_identity(&self) -> Option<&str> {
        self.run_as_identity_id
            .as_deref()
            .or(self.creator_identity_id.as_deref())
            .filter(|s| !s.is_empty())
    }

    /// The person a suspension holds this job for: whom it runs as, or a
    /// reminder's recipient when it names no creator. `None` for the jobs the
    /// hub registers itself (workflow schedules, monitor sweeps,
    /// consolidation), whose own runtimes refuse a suspended person and which
    /// a re-registration would switch back on.
    pub fn held_for(&self) -> Option<&str> {
        match &self.payload {
            JobPayload::WorkflowStart { .. }
            | JobPayload::MonitorSweep
            | JobPayload::Consolidation { .. } => None,
            JobPayload::Reminder { .. } => {
                self.effective_run_as_identity().or_else(|| self.owner())
            }
            JobPayload::AgentTurn { .. } | JobPayload::SystemEvent { .. } => {
                self.effective_run_as_identity()
            }
        }
    }

    /// Switch the job off while the person it runs for is suspended.
    /// Returns the fields it changed.
    pub fn hold(&mut self) -> &'static [&'static str] {
        self.enabled = false;
        self.paused_by_suspension = true;
        &["enabled", "paused_by_suspension"]
    }

    /// Switch a held job back on, at its next occurrence from `now`: the
    /// occurrences it missed while held are not made up. A one-shot keeps
    /// its time, so one that fell due while held fires now.
    pub fn release(&mut self, now: &DateTime<Utc>, default_tz: Tz) -> &'static [&'static str] {
        self.enabled = true;
        self.paused_by_suspension = false;
        if !matches!(self.schedule, Schedule::At { .. }) {
            self.next_run = next_run_time(&self.schedule, now, default_tz);
        }
        &["enabled", "paused_by_suspension", "next_run"]
    }

    /// Identity whose budget this job's spend counts against: an explicit
    /// bill-to, else whoever the job runs as.
    pub fn effective_billing_identity(&self) -> Option<&str> {
        self.bill_to_identity_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.effective_run_as_identity())
    }

    /// The session this job keeps across its runs, if it keeps one: the
    /// session its stored key names, kept apart for the person it runs as.
    /// The key is minted for the job when it is pinned; one an earlier
    /// version stored (a conversation's key) is still never shared between
    /// people. Every hub version that scopes per person resolves the same.
    pub fn pinned_session(&self) -> Option<String> {
        match &self.payload {
            JobPayload::AgentTurn {
                channel: Some(channel),
                ..
            } => Some(continued_session_channel(
                channel,
                self.effective_run_as_identity(),
            )),
            _ => None,
        }
    }

    /// The session its runs use; `None` for a job that runs no agent turn.
    pub fn session_mode(&self) -> Option<SessionMode> {
        match &self.payload {
            JobPayload::AgentTurn { channel, .. } => Some(match channel {
                Some(_) => SessionMode::Pinned,
                None => SessionMode::Fresh,
            }),
            _ => None,
        }
    }

    /// Resolved visibility: reminders are always private; otherwise an explicit
    /// value wins, owned jobs default to private, and ownerless (system/legacy)
    /// jobs default to global so pre-existing jobs stay visible.
    pub fn effective_visibility(&self) -> &str {
        if self.is_reminder {
            return VISIBILITY_PRIVATE;
        }
        match self.visibility.as_deref() {
            Some(VISIBILITY_GLOBAL) => VISIBILITY_GLOBAL,
            Some(VISIBILITY_PRIVATE) => VISIBILITY_PRIVATE,
            _ if self.owner().is_some() => VISIBILITY_PRIVATE,
            _ => VISIBILITY_GLOBAL,
        }
    }
}

/// Which session an agent-turn job's runs use. Never stored: a job is
/// pinned exactly when its payload names a session, so every hub version
/// reads the same mode off the same stored job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// A new session every run: no run sees another's transcript.
    #[serde(alias = "isolated")]
    Fresh,
    /// One session across runs: every run replays all the runs before it.
    #[serde(alias = "conversation")]
    Pinned,
}

/// The edits [`Scheduler::update_job`] applies; an unset field is left as it
/// is.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct JobEdit {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub schedule: Option<Schedule>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub session: Option<SessionMode>,
    /// The only tools the job's runs are offered; an empty list restores the
    /// agent's full surface.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// The user has seen what the edit is forecast to cost and approved it.
    #[serde(default)]
    pub confirmed: bool,
}

/// What keeping one session across runs costs, in the words the user is
/// asked to approve it with.
pub const PINNED_COST: &str = "every run replays all the job's earlier runs, so each run \
                                      costs more than the one before";

/// What a recurring job is forecast to cost. The user approves it before
/// the job is created or changed when it [`Self::needs_confirmation`].
#[derive(Debug, Clone)]
pub struct RecurringCost {
    /// The job is to keep one session across runs, which always asks.
    pub pins: bool,
    pub per_run: crate::hub_core::cost_forecast::CostForecast,
    pub projected: crate::hub_core::cost_forecast::RecurringForecast,
    pub threshold_usd: f64,
}

impl RecurringCost {
    /// Whether the job keeps a session, or its projected daily cost is over
    /// `[budget] confirm_recurring_daily_usd`.
    pub fn needs_confirmation(&self) -> bool {
        self.pins || self.projected.per_day_usd > self.threshold_usd
    }

    /// Why approval is asked and the forecast, as every surface shows them.
    pub fn to_json(&self) -> serde_json::Value {
        use crate::hub_core::cost_forecast::round2;
        let projected = &self.projected;
        serde_json::json!({
            "status": "needs_confirmation",
            "reason": self.to_string(),
            "estimate": {
                "per_run_usd": round2(projected.per_run_usd),
                "runs_per_day": (projected.runs_per_day * 10.0).round() / 10.0,
                "per_day_usd": round2(projected.per_day_usd),
                "per_month_usd": round2(projected.per_month_usd),
                "basis": self.per_run.basis,
                "priced": self.per_run.confidence > 0.3,
            },
        })
    }
}

impl std::fmt::Display for RecurringCost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.pins {
            true => write!(
                f,
                "conversation context always requires user confirmation: {PINNED_COST}"
            ),
            false => write!(
                f,
                "projected ${:.2}/day exceeds the ${:.2}/day confirmation threshold",
                self.projected.per_day_usd, self.threshold_usd
            ),
        }
    }
}

/// Why [`Scheduler::update_job`] did not apply an edit.
#[derive(Debug)]
pub enum EditRefused {
    /// The job cannot take the edit.
    Invalid(String),
    /// The edit makes the job cost more and was not sent `confirmed`.
    NeedsConfirmation(RecurringCost),
}

impl std::fmt::Display for EditRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(why) => f.write_str(why),
            Self::NeedsConfirmation(cost) => cost.fmt(f),
        }
    }
}

impl JobEdit {
    /// Whether the edit moves `job` onto a session of its own.
    pub fn pins(&self, job: &ScheduledJob) -> bool {
        self.session == Some(SessionMode::Pinned) && job.session_mode() == Some(SessionMode::Fresh)
    }

    /// Whether the edit changes what the job does or who may read it, which
    /// makes whoever saves it the one in charge of it
    /// ([`ScheduledJob::edited_by`]). Renaming and pausing do not.
    pub fn changes_control(&self) -> bool {
        self.schedule.is_some()
            || self.visibility.is_some()
            || self.session.is_some()
            || self.tools.is_some()
    }

    /// The top-level fields of a stored job the edit writes.
    pub fn fields(&self) -> Vec<&'static str> {
        [
            (self.name.is_some(), &["name"][..]),
            (self.schedule.is_some(), &["schedule", "next_run"]),
            (self.enabled.is_some(), &["enabled", "paused_by_suspension"]),
            (self.visibility.is_some(), &["visibility"]),
            (self.session.is_some() || self.tools.is_some(), &["payload"]),
        ]
        .into_iter()
        .filter(|(edited, _)| *edited)
        .flat_map(|(_, fields)| fields.iter().copied())
        .collect()
    }

    /// Write the edit onto `job`, which it has been checked against.
    pub fn apply(self, job: &mut ScheduledJob, now: &DateTime<Utc>, default_tz: Tz) {
        let pins = self.pins(job);
        if let Some(name) = self.name {
            job.name = name;
        }
        if let Some(schedule) = self.schedule {
            job.next_run = next_run_time(&schedule, now, default_tz);
            job.schedule = schedule;
        }
        if let Some(enabled) = self.enabled {
            job.enabled = enabled;
            job.paused_by_suspension = false;
        }
        if let Some(visibility) = self.visibility {
            job.visibility = Some(visibility);
        }
        if let JobPayload::AgentTurn { channel, tools, .. } = &mut job.payload {
            match self.session {
                Some(SessionMode::Fresh) => *channel = None,
                Some(SessionMode::Pinned) if pins => {
                    *channel = Some(pinned_session_channel(&job.id));
                }
                Some(SessionMode::Pinned) | None => {}
            }
            if let Some(offered) = self.tools {
                *tools = Some(offered).filter(|offered| !offered.is_empty());
            }
        }
    }
}

/// Schedule types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Schedule {
    /// One-shot at a specific time.
    #[serde(rename = "at")]
    At { at: DateTime<Utc> },
    /// Recurring interval.
    #[serde(rename = "every")]
    Every {
        every_secs: u64,
        #[serde(default)]
        anchor: Option<DateTime<Utc>>,
    },
    /// Cron expression.
    #[serde(rename = "cron")]
    Cron {
        expr: String,
        #[serde(default)]
        tz: Option<String>,
    },
}

/// What to do when the job fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum JobPayload {
    /// Inject a system event into the agent's session.
    #[serde(rename = "system_event")]
    SystemEvent {
        text: String,
        /// Slack channel ID to deliver the message to.
        #[serde(default)]
        channel_id: Option<String>,
        /// Slack thread timestamp for threaded replies.
        #[serde(default)]
        thread_id: Option<String>,
        /// Slack user ID for DM channel recovery after token rotation.
        #[serde(default)]
        user_id: Option<String>,
    },
    /// Run the agent with a message in an isolated session.
    #[serde(rename = "agent_turn")]
    AgentTurn {
        message: String,
        #[serde(default)]
        channel: Option<String>,
        /// Slack channel ID to deliver the response to.
        #[serde(default)]
        channel_id: Option<String>,
        /// Slack thread timestamp for threaded replies.
        #[serde(default)]
        thread_id: Option<String>,
        /// Override the default model for this job.
        #[serde(default)]
        model: Option<String>,
        /// Slack user ID for DM channel recovery after token rotation.
        #[serde(default)]
        user_id: Option<String>,
        /// Cap the runner's max_rounds for this one job. Defends against the
        /// agent looping/polling on background work (e.g. consolidation).
        #[serde(default)]
        max_rounds: Option<u32>,
        /// When `true`, the agent self-posts its result during the turn (e.g.
        /// via `messaging.send`) and the scheduler does NOT additionally deliver
        /// the agent's final text. Prevents the duplicate "report + chatty
        /// `Done — posted …` confirmation" double-post on report-style jobs.
        /// Default `false` preserves the legacy "scheduler delivers the reply"
        /// behaviour for every existing job, and leaves fan-out jobs (which post
        /// to several channels themselves) free to keep doing so.
        #[serde(default)]
        agent_owns_delivery: bool,
        /// The only tools the job's turn is offered, by name or namespace.
        /// Unset keeps the agent's full surface.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools: Option<Vec<String>>,
    },
    /// Send a direct reminder to a user via DM. No agent involvement —
    /// the scheduler delivers the message directly, making it 100% reliable.
    #[serde(rename = "reminder")]
    Reminder {
        /// The reminder message text to send to the user.
        text: String,
        /// Slack user ID — always open a DM with this user.
        user_id: String,
    },
    /// Start a workflow run. Used by `[[trigger]] type = "schedule"` entries
    /// in workflow TOML — `workflow_runtime::sync_schedule_triggers` registers
    /// one of these per schedule at workflow load time.
    ///
    /// Per-user scoping is handled by the outer `ScheduledJob.run_as_identity_id`
    /// field (set from the trigger's `run_as`), so `CURRENT_USER_ID` is in
    /// scope for the spawned workflow's tool calls.
    #[serde(rename = "workflow_start")]
    WorkflowStart {
        workflow_id: String,
        #[serde(default)]
        payload: serde_json::Value,
        #[serde(default)]
        channel_id: Option<String>,
        #[serde(default)]
        thread_id: Option<String>,
    },
    /// Run one channel-monitor catch-up pass (sweep + recover + prune). A single
    /// recurring job registered at startup; leader-gated like every other job.
    #[serde(rename = "monitor_sweep")]
    MonitorSweep,
    /// Run one memory-consolidation pass for `identity_id` (`None` = the
    /// agent-global pass). The engine (`model` vs `claude_code`) and prompt
    /// resolve at execution time from the live `[memory]` config, so flipping
    /// `consolidation_source` needs no job re-registration.
    #[serde(rename = "consolidation")]
    Consolidation {
        #[serde(default)]
        identity_id: Option<String>,
    },
}

/// A record of a single job run.
impl JobPayload {
    /// The person the payload acts for or delivers to, when it names one.
    pub fn user_id(&self) -> Option<&str> {
        match self {
            Self::SystemEvent { user_id, .. } | Self::AgentTurn { user_id, .. } => {
                user_id.as_deref()
            }
            Self::Reminder { user_id, .. } => Some(user_id),
            Self::Consolidation { identity_id } => identity_id.as_deref(),
            Self::WorkflowStart { .. } | Self::MonitorSweep => None,
        }
    }

    /// Point the payload at `to` when it names anyone in `names`. Delivery
    /// maps an identity to its platform id, so `to` is always an identity.
    fn reassign_user(&mut self, names: &[String], to: &str) -> bool {
        let slot = match self {
            Self::SystemEvent { user_id, .. }
            | Self::AgentTurn { user_id, .. }
            | Self::Consolidation {
                identity_id: user_id,
            } => user_id.as_mut(),
            Self::Reminder { user_id, .. } => Some(user_id),
            Self::WorkflowStart { .. } | Self::MonitorSweep => None,
        };
        match slot {
            Some(id) if names.contains(id) => {
                *id = to.to_string();
                true
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRunRecord {
    pub job_id: String,
    pub job_name: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub success: bool,
    /// Truncated output (first 500 chars).
    pub output: String,
    pub delivered: bool,
    /// What the run did other than the job as stored asks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// How a job's latest runs went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Outcomes {
    /// The runs classified: fewer than asked for when the hub holds fewer.
    pub considered: usize,
    pub started: usize,
    /// Slots skipped because the workflow was at its `max_concurrent`.
    pub skipped: usize,
    pub failed: usize,
}

/// The session a job that continues one across runs (`channel`) runs in: the
/// named one for the agent's own work, and one apart for each person a job
/// runs as, so no run replays what another person's run fetched. A person's
/// is a scheduler channel like any other run's, with the name last: the
/// name may be a conversation's key, whose own shape its readers rely on.
pub fn continued_session_channel(channel: &str, run_as: Option<&str>) -> String {
    match run_as {
        Some(person) => format!("scheduler:{person}:{channel}"),
        None => channel.to_string(),
    }
}

pub const PINNED_SESSION_PREFIX: &str = "scheduler:pinned:";

/// A session channel for the job `job_id` to keep across its runs. Minted per
/// pin and never chosen by a caller, so no two jobs share a transcript and a
/// job pinned again starts empty rather than back in the transcript it left.
pub fn pinned_session_channel(job_id: &str) -> String {
    format!("{PINNED_SESSION_PREFIX}{job_id}:{}", Uuid::new_v4())
}

/// Expected fires per day, for cost projection. Cron cadence is measured from
/// the gap between the next two occurrences; a one-shot is zero.
pub fn runs_per_day(schedule: &Schedule, now: &DateTime<Utc>, default_tz: Tz) -> f64 {
    match schedule {
        Schedule::At { .. } => 0.0,
        Schedule::Every { every_secs, .. } => {
            if *every_secs == 0 {
                0.0
            } else {
                86_400.0 / *every_secs as f64
            }
        }
        Schedule::Cron { expr, tz } => {
            let Some(first) = cron_next_run(expr, tz.as_deref(), now, default_tz) else {
                return 0.0;
            };
            let probe = first + chrono::Duration::seconds(1);
            match cron_next_run(expr, tz.as_deref(), &probe, default_tz) {
                Some(second) => {
                    let gap = (second - first).num_seconds().max(1) as f64;
                    (86_400.0 / gap).min(86_400.0)
                }
                None => 0.0,
            }
        }
    }
}

pub fn next_run_time(
    schedule: &Schedule,
    now: &DateTime<Utc>,
    default_tz: Tz,
) -> Option<DateTime<Utc>> {
    match schedule {
        Schedule::At { at } => {
            if at > now {
                Some(*at)
            } else {
                None
            }
        }
        Schedule::Every { every_secs, anchor } => {
            let base = anchor.unwrap_or(*now);
            let interval = chrono::Duration::seconds(*every_secs as i64);

            if base > *now {
                Some(base)
            } else {
                // Find next occurrence after now
                let elapsed = (*now - base).num_seconds();
                let secs = *every_secs as i64;
                let intervals = elapsed / secs.max(1) + 1;
                Some(base + interval * intervals as i32)
            }
        }
        Schedule::Cron { expr, tz } => cron_next_run(expr, tz.as_deref(), now, default_tz),
    }
}

/// Compute the next occurrence of a cron expression after `now`.
///
/// The cron expression is evaluated in the timezone specified by `tz_name`,
/// falling back to `default_tz`. The result is converted back to UTC.
pub fn cron_next_run(
    expr: &str,
    tz_name: Option<&str>,
    now: &DateTime<Utc>,
    default_tz: Tz,
) -> Option<DateTime<Utc>> {
    let tz = tz_name
        .and_then(|s| s.parse::<Tz>().ok())
        .unwrap_or(default_tz);

    let cron = match croner::Cron::new(expr).parse() {
        Ok(c) => c,
        Err(e) => {
            warn!(expr = expr, error = %e, "Invalid cron expression");
            return None;
        }
    };

    // Convert now to the target timezone, find next occurrence, convert back
    let now_in_tz = now.with_timezone(&tz);
    cron.find_next_occurrence(&now_in_tz, false)
        .ok()
        .map(|next_local| next_local.with_timezone(&Utc))
}

/// Whether `session` is a person's continued job session
/// ([`continued_session_channel`]) continuing a conversation of a
/// conversational surface (web, iOS, terminal), which their runs still read
/// as that conversation does.
pub fn continues_conversation(session: &str) -> bool {
    session
        .strip_prefix("scheduler:")
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(_, key)| workforce_common::channels::is_conversational(key))
}
