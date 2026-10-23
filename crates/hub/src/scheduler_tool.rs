use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};

use crate::authz::{always_admin, IsAdminFn};
use crate::hub_core::schedules::{
    EditRefused, JobEdit, JobPayload, RecurringCost, Schedule, ScheduledJob, SessionMode,
    PINNED_COST,
};
use crate::scheduler::{schedule_mutable_by, schedule_visible_to, Scheduler};
use crate::user_config::current_identity_id;

/// Opaque refusal for a mutate/read op on a job the caller doesn't own. Used
/// identically for "not found" and "owned by someone else" so the agent (and a
/// curious member) can't probe another user's job IDs for existence.
const NOT_YOURS: &str = "Job not found or you can only manage your own scheduled jobs";

/// Tool that lets agents create, list, remove, and run scheduled jobs.
pub struct SchedulerTool {
    scheduler: Arc<Scheduler>,
    /// Owner-scope admin gate. An admin sees and mutates every job; everyone
    /// else is restricted to jobs they own (plus visibility of non-reminder
    /// global jobs). Defaults to `always_admin` for single-tenant/CLI use and
    /// existing tests; `serve.rs` threads the real identity-backed policy.
    is_admin: IsAdminFn,
}

impl SchedulerTool {
    pub fn new(scheduler: Arc<Scheduler>) -> Self {
        Self {
            scheduler,
            is_admin: always_admin(),
        }
    }

    /// Inject the real admin policy (the same `IsAdminFn` the vault tools get).
    pub fn with_admin_policy(mut self, is_admin: IsAdminFn) -> Self {
        self.is_admin = is_admin;
        self
    }

    /// Look up a job and require the caller may mutate it (admin or owner).
    /// Returns the same opaque error for not-found and not-owned so a member
    /// cannot probe another user's job IDs. `Ok(Some(job))` ⇒ proceed.
    async fn job_if_mutable(&self, job_id: &str) -> Result<ScheduledJob, ToolResult> {
        let caller = current_identity_id();
        let is_admin = (self.is_admin)();
        match self.scheduler.get_job(job_id).await {
            Some(job) if schedule_mutable_by(&job, caller.as_deref(), is_admin) => Ok(job),
            _ => Err(ToolResult::failure(NOT_YOURS, 0)),
        }
    }
}

#[derive(Deserialize)]
struct SchedulerInput {
    action: String,
    /// Identity to bill this job's spend to (set_bill_to; empty string clears).
    #[serde(default)]
    bill_to: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    message: Option<String>,
    /// Relative time: "2m", "1h", "30s", "1d"
    #[serde(default)]
    in_time: Option<String>,
    /// Absolute ISO 8601 datetime
    #[serde(default)]
    at: Option<String>,
    /// Recurring interval in seconds
    #[serde(default)]
    every_secs: Option<u64>,
    /// Cron expression
    #[serde(default)]
    cron: Option<String>,
    /// Timezone for cron expression (IANA name, e.g. "Europe/London")
    #[serde(default)]
    timezone: Option<String>,
    /// Job ID (for update, remove, run, history)
    #[serde(default)]
    job_id: Option<String>,
    /// What earlier versions took for a session key and models sent for
    /// the delivery channel. Accepted only as the latter.
    #[serde(default)]
    channel: Option<String>,
    /// Pause or resume the job (update)
    #[serde(default)]
    enabled: Option<bool>,
    /// "private" or "global" (update)
    #[serde(default)]
    visibility: Option<String>,
    /// Slack channel ID for delivering the result (from messaging context)
    #[serde(default)]
    channel_id: Option<String>,
    /// Slack thread timestamp for threaded delivery (from messaging context)
    #[serde(default)]
    thread_id: Option<String>,
    /// Model override for agent_turn payloads
    #[serde(default)]
    model: Option<String>,
    /// When the job's instruction tells the agent to POST its result to a
    /// channel itself (e.g. "post the deploy report to #eng"), set this so the
    /// scheduler does NOT also deliver the agent's reply — preventing a
    /// duplicate report + a chatty "Done" confirmation. Leave unset/false for
    /// normal "do X and tell me the result" jobs where the scheduler delivers
    /// the reply.
    #[serde(default)]
    agent_owns_delivery: Option<bool>,
    /// The only tools the job's runs are offered, by name or namespace.
    #[serde(default)]
    tools: Option<Vec<String>>,
    /// Session the job's runs use: "isolated" (default, fresh every run) or
    /// "conversation" (one session minted for the job alone, which re-bills
    /// every earlier run on each run).
    #[serde(default)]
    context: Option<SessionMode>,
    /// Auto-disable after this many runs. May only tighten the configured
    /// default.
    #[serde(default)]
    max_runs: Option<u64>,
    /// Auto-disable after this relative time (e.g. "3d", "12h"). May only
    /// tighten the configured default.
    #[serde(default)]
    expires_in: Option<String>,
    /// Set true ONLY after the user explicitly approved a `needs_confirmation`
    /// estimate for this creation.
    #[serde(default)]
    confirmed: Option<bool>,
}

use crate::cost_forecast::round2;

/// Who a save through the tool is made as: the caller in scope.
fn editor() -> String {
    current_identity_id().unwrap_or_else(|| crate::scheduler::UNIDENTIFIED_EDITOR.to_string())
}

/// The tool's name for a session mode, as its `context` parameter spells it.
fn context_name(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::Fresh => "isolated",
        SessionMode::Pinned => "conversation",
    }
}

/// The schedule `input` names, if it names one.
fn schedule_of(input: &SchedulerInput) -> Result<Option<Schedule>, String> {
    if let Some(in_time) = input.in_time.as_deref() {
        return parse_relative_time(in_time)
            .map(|duration| {
                Some(Schedule::At {
                    at: Utc::now() + duration,
                })
            })
            .ok_or_else(|| {
                format!(
                    "Could not parse relative time '{in_time}'. Use format like '2m', '1h', '30s', '1d'"
                )
            });
    }
    if let Some(at) = input.at.as_deref() {
        return at
            .parse::<chrono::DateTime<Utc>>()
            .map(|at| Some(Schedule::At { at }))
            .map_err(|e| format!("Could not parse datetime '{at}': {e}"));
    }
    Ok(input
        .every_secs
        .map(|every_secs| Schedule::Every {
            every_secs,
            anchor: None,
        })
        .or_else(|| {
            input.cron.clone().map(|expr| Schedule::Cron {
                expr,
                tz: input.timezone.clone(),
            })
        }))
}

/// What `list` and `update` say of one job.
fn job_summary(job: &ScheduledJob) -> serde_json::Value {
    let tools = match &job.payload {
        JobPayload::AgentTurn { tools, .. } => tools.as_deref(),
        _ => None,
    };
    serde_json::json!({
        "id": job.id,
        "name": job.name,
        "description": job.description,
        "enabled": job.enabled,
        "schedule": job.schedule,
        "visibility": job.effective_visibility(),
        "context": job.session_mode().map(context_name),
        "tools": tools,
        "next_run": job.next_run.map(|t| t.to_rfc3339()),
        "last_run": job.last_run.map(|t| t.to_rfc3339()),
        "run_count": job.run_count,
        // Present only on jobs created by voice. Surfaced so
        // "why did my 8am summary stop reaching Slack?" is
        // answerable without reading the job store.
        "deliver_to": job.deliver_to,
    })
}

/// The structured estimate an over-threshold recurring job returns instead
/// of being created or changed (`verb`). The model presents it to the user;
/// only explicit user approval justifies re-calling with `confirmed: true`.
fn needs_confirmation_payload(verb: &str, name: &str, cost: &RecurringCost) -> serde_json::Value {
    let cadence_option = |label: &str, secs: f64| {
        serde_json::json!({
            "option": format!("run {label} instead"),
            "estimated_per_day_usd": round2(cost.per_run.estimated_cost_usd * (86_400.0 / secs)),
        })
    };
    let keep_isolated = cost.pins.then(|| {
        serde_json::json!({
            "option": "keep the default isolated context",
            "detail": PINNED_COST,
        })
    });
    let alternatives: Vec<serde_json::Value> = keep_isolated
        .into_iter()
        .chain([
            serde_json::json!({
                "option": "use an event-driven monitor instead of polling",
                "detail": "a monitor reacts per matching message in a small isolated session — the right tool for 'tell me when someone replies / something changes'",
            }),
            cadence_option("hourly", 3600.0),
            cadence_option("every 6 hours", 21_600.0),
            cadence_option("daily", 86_400.0),
            serde_json::json!({
                "option": "pin a cheaper model for this job via the `model` parameter",
            }),
            serde_json::json!({
                "option": "use a one-shot (`in_time`/`at`) plus a tighter `max_runs`/`expires_in` if it must poll",
            }),
        ])
        .collect();
    let mut payload = cost.to_json();
    payload["job_name"] = name.into();
    payload["alternatives"] = alternatives.into();
    payload["instruction"] = format!(
        "Present this estimate and the alternatives to the user. {verb} the job (re-call with \
         `confirmed: true`) ONLY if the user explicitly approves; never confirm on their behalf."
    )
    .into();
    payload
}

/// Parse a relative time string like "2m", "1h30m", "30s", "1d" into a Duration.
pub fn parse_relative_time(s: &str) -> Option<Duration> {
    let s = s.trim().to_lowercase();
    let mut total_secs: i64 = 0;
    let mut num_buf = String::new();

    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num_buf.push(ch);
        } else {
            let n: i64 = num_buf.parse().ok()?;
            num_buf.clear();
            match ch {
                's' => total_secs += n,
                'm' => total_secs += n * 60,
                'h' => total_secs += n * 3600,
                'd' => total_secs += n * 86400,
                _ => return None,
            }
        }
    }

    // If there's a bare number left with no unit, treat as seconds
    if !num_buf.is_empty() {
        let n: i64 = num_buf.parse().ok()?;
        total_secs += n;
    }

    if total_secs > 0 {
        Some(Duration::seconds(total_secs))
    } else {
        None
    }
}

#[async_trait]
impl Tool for SchedulerTool {
    fn name(&self) -> &str {
        "scheduler"
    }

    fn description(&self) -> &str {
        "Manage scheduled jobs, reminders, and recurring tasks. Recurring jobs cost real money: \
         every fire is a full agent turn. Before creating one, prefer (in order) an event-driven \
         monitor over polling, the coarsest interval that works, and an explicit `max_runs` or \
         `expires_in` for anything that watches for a condition. Recurring intervals have a \
         configured floor; a create or update projected above the configured daily cost returns \
         a `needs_confirmation` estimate that the USER must approve before you re-call with \
         `confirmed: true`. `context: \"isolated\"` (the default) runs the job in a fresh \
         session every time: no run sees an earlier one and every run costs about the same. \
         `context: \"conversation\"` keeps one session that belongs to the job alone and \
         starts empty (it is NOT this conversation): every run replays the job's earlier runs, \
         so each run costs more than the one before, until the session is cut back to a short \
         tail of its last runs; it always requires confirmation. Where the result is delivered \
         (`channel_id`) never changes which session a job runs in. `list` shows each job's \
         context and run count, and what your own jobs have spent; `update` changes a job in \
         place, including moving it back to `isolated`."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "update", "remove", "run", "history", "set_bill_to"],
                    "description": "Action to perform"
                },
                "name": {
                    "type": "string",
                    "description": "Human-readable name for the job (for create, update)"
                },
                "bill_to": {
                    "type": "string",
                    "description": "SysAdmin only (set_bill_to): identity charged for the job's spend — typically a system:* lane so background automation doesn't drain the owner's personal budget. Empty string reverts to billing the run-as identity."
                },
                "description": {
                    "type": "string",
                    "description": "Longer description of the job's purpose (for create)"
                },
                "message": {
                    "type": "string",
                    "description": "The instruction for the agent when this job fires"
                },
                "in_time": {
                    "type": "string",
                    "description": "Relative time from now"
                },
                "at": {
                    "type": "string",
                    "description": "Absolute ISO 8601 datetime for one-shot (for create)"
                },
                "every_secs": {
                    "type": "integer",
                    "description": "Recurring interval in seconds (for create, update). Subject to the configured minimum; prefer the coarsest interval that works."
                },
                "cron": {
                    "type": "string",
                    "description": "Cron expression for recurring schedule (for create, update). Subject to the same minimum-interval floor as every_secs."
                },
                "timezone": {
                    "type": "string",
                    "description": "IANA timezone (default: UTC)"
                },
                "job_id": {
                    "type": "string",
                    "description": "Job ID (for update, remove, run, history)"
                },
                "enabled": {
                    "type": "boolean",
                    "description": "Pause (false) or resume (true) the job (for update)"
                },
                "visibility": {
                    "type": "string",
                    "enum": ["private", "global"],
                    "description": "Who may see the job: only its owner and admins, or everyone (for update)"
                },
                "channel_id": {
                    "type": "string",
                    "description": "Slack channel ID for delivering the result. Delivery only: it has no effect on which session the job runs in."
                },
                "thread_id": {
                    "type": "string",
                    "description": "Slack thread timestamp for threaded delivery"
                },
                "model": {
                    "type": "string",
                    "description": "Model override for agent_turn jobs (optional, for create)"
                },
                "agent_owns_delivery": {
                    "type": "boolean",
                    "description": "Set true when the job's instruction tells the agent to post its result to a channel itself, so the scheduler won't ALSO deliver the agent's reply (avoids a duplicate report + a chatty 'Done' confirmation). Default false: the scheduler delivers the reply."
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Only these tools are offered to the job's runs, by exact name or namespace (e.g. [\"github\", \"messaging.send\"]). A narrow set makes every run cheaper; include every tool the instruction needs. Omit for the agent's full toolset (for create). On update, replaces the job's list; [] restores the full toolset."
                },
                "context": {
                    "type": "string",
                    "enum": ["isolated", "conversation"],
                    "description": "Session the job's runs use (for create, update). Leave unset on create for 'isolated': a fresh session every run, the cheap default. 'conversation' keeps one session of the job's own, empty at first and never this conversation, so every run replays the job's earlier runs and costs more than the one before; ask for it only when the user wants the job to remember its previous runs, and expect a needs_confirmation reply. On update, 'isolated' moves a job back to fresh sessions and 'conversation' gives a job that has no session of its own a new, empty one."
                },
                "max_runs": {
                    "type": "integer",
                    "description": "Auto-disable the job after this many runs (for create). Can only tighten the configured default."
                },
                "expires_in": {
                    "type": "string",
                    "description": "Auto-disable the job after this relative time, e.g. '3d', '12h' (for create). Can only tighten the configured default."
                },
                "confirmed": {
                    "type": "boolean",
                    "description": "Set true ONLY after the user explicitly approved the needs_confirmation reply returned by a previous create or update attempt."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: SchedulerInput = serde_json::from_value(input)
            .map_err(|e| eyre::eyre!("Invalid scheduler input: {}", e))?;

        match input.action.as_str() {
            "create" => self.handle_create(input, start).await,
            "list" => self.handle_list(start).await,
            "update" => self.handle_update(input, start).await,
            "remove" => self.handle_remove(input, start).await,
            "run" => self.handle_run(input, start).await,
            "history" => self.handle_history(input, start).await,
            "set_bill_to" => self.handle_set_bill_to(input, start).await,
            other => Ok(ToolResult::failure(
                format!(
                    "Unknown action '{}'. Use: create, list, update, remove, run, history, set_bill_to",
                    other
                ),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

impl SchedulerTool {
    async fn handle_create(&self, input: SchedulerInput, start: Instant) -> Result<ToolResult> {
        let schedule = schedule_of(&input);
        let message = match input.message {
            Some(m) if !m.is_empty() => m,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'message' — what should the reminder say?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let name = input
            .name
            .unwrap_or_else(|| format!("reminder-{}", &Utc::now().timestamp()));

        let schedule = match schedule {
            Ok(Some(schedule)) => schedule,
            Ok(None) => {
                return Ok(ToolResult::failure(
                    "No schedule specified. Use 'in_time' (e.g. '2m'), 'at' (ISO datetime), 'every_secs', or 'cron'.",
                    start.elapsed().as_millis() as u64,
                ));
            }
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };

        // Resolve channel_id/thread_id — prefer explicit input, fall back to
        // the messaging context captured by process_message before this agent run.
        let (resolved_channel_id, resolved_thread_id) = if input.channel_id.is_some() {
            (input.channel_id, input.thread_id)
        } else {
            let session_key = workforce_agent::runner::CURRENT_SESSION_KEY
                .try_with(|k| k.clone())
                .unwrap_or_default();
            match self.scheduler.get_messaging_context(&session_key).await {
                Some((cid, tid)) => (Some(cid), tid),
                None => (None, input.thread_id),
            }
        };

        if input
            .channel
            .as_deref()
            .is_some_and(|channel| Some(channel) != resolved_channel_id.as_deref())
        {
            return Ok(ToolResult::failure(
                "`channel` is not a parameter. Use `channel_id` for where the result is \
                 delivered, and `context: \"conversation\"` only if the job must keep one \
                 session across its runs.",
                start.elapsed().as_millis() as u64,
            ));
        }

        let service = self.scheduler.service();
        let budget = &service.config.budget;
        let tz = service.config.user_timezone();
        let now = Utc::now();
        let recurring = !matches!(schedule, Schedule::At { .. });

        let requested_expiry = match input.expires_in.as_deref() {
            Some(raw) => match parse_relative_time(raw) {
                Some(d) => Some(now + d),
                None => {
                    return Ok(ToolResult::failure(
                        format!(
                            "Could not parse 'expires_in' '{raw}'. Use format like '12h', '3d'"
                        ),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            },
            None => None,
        };

        // Pre-validate the floor before the estimate flow: without this, a
        // below-floor schedule could sail through `needs_confirmation`, get
        // the user's approval, and only THEN be rejected by `insert_job`.
        if budget.enabled && recurring {
            if let Err(e) = crate::scheduler::validate_schedule(
                &schedule,
                budget.min_recurring_interval_secs,
                &now,
                tz,
            ) {
                return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64));
            }
        }

        // A job is pinned only when asked for by name. Where its result is
        // delivered says nothing about the session it runs in.
        let pinned = input.context == Some(SessionMode::Pinned);
        if pinned && !self.scheduler.may_pin(&schedule) {
            return Ok(ToolResult::failure(
                "This agent's config does not let recurring jobs keep a session across runs. \
                 Create the job with the default isolated context.",
                start.elapsed().as_millis() as u64,
            ));
        }

        let estimate = self
            .scheduler
            .recurring_cost(&schedule, input.model.as_deref(), pinned)
            .await;
        if let Some(cost) = estimate
            .as_ref()
            .filter(|cost| cost.needs_confirmation() && input.confirmed != Some(true))
        {
            return Ok(ToolResult::success(
                needs_confirmation_payload("Create", &name, cost),
                start.elapsed().as_millis() as u64,
            ));
        }

        // Capture user_id for DM channel recovery after token rotation.
        let resolved_user_id = workforce_agent::runner::CURRENT_USER_ID
            .try_with(|u| u.clone())
            .ok()
            .filter(|u| !u.is_empty());

        // Always use AgentTurn so the agent runs when the schedule fires
        // (can check CI, merge PRs, fix builds, etc.)
        let payload = JobPayload::AgentTurn {
            message: message.clone(),
            channel: None,
            channel_id: resolved_channel_id.clone(),
            thread_id: resolved_thread_id.clone(),
            model: input.model,
            user_id: resolved_user_id,
            max_rounds: None,
            agent_owns_delivery: input.agent_owns_delivery.unwrap_or(false),
            tools: input.tools,
        };

        let job_id = match self
            .scheduler
            .add_job_with_desc(
                &name,
                input.description.as_deref(),
                schedule.clone(),
                payload,
            )
            .await
        {
            Ok(id) => id,
            Err(e) => {
                return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64));
            }
        };

        if input.max_runs.is_some() || requested_expiry.is_some() {
            self.scheduler
                .tighten_job_caps(&job_id, input.max_runs, None, requested_expiry)
                .await;
        }
        if pinned {
            let pin = JobEdit {
                session: Some(SessionMode::Pinned),
                confirmed: true,
                ..Default::default()
            };
            if let Err(e) = self.scheduler.update_job(&job_id, pin, &editor()).await {
                self.scheduler.remove_job(&job_id).await;
                return Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }
        }

        let job = self.scheduler.get_job(&job_id).await;
        let next_run = match &schedule {
            Schedule::At { at } => at.to_rfc3339(),
            Schedule::Every { every_secs, .. } => format!("every {}s", every_secs),
            Schedule::Cron { expr, tz } => {
                if let Some(tz) = tz {
                    format!("cron: {} ({})", expr, tz)
                } else {
                    format!("cron: {}", expr)
                }
            }
        };

        Ok(ToolResult::success(
            serde_json::json!({
                "job_id": job_id,
                "name": name,
                "message": message,
                "schedule": next_run,
                "context": context_name(input.context.unwrap_or(SessionMode::Fresh)),
                "max_runs": job.as_ref().and_then(|j| j.max_runs),
                "budget_usd": job.as_ref().and_then(|j| j.budget_usd),
                "expires_at": job.as_ref().and_then(|j| j.expires_at).map(|t| t.to_rfc3339()),
                "estimated_per_day_usd": estimate.as_ref().map(|cost| round2(cost.projected.per_day_usd)),
                "status": "created"
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    async fn handle_list(&self, start: Instant) -> Result<ToolResult> {
        let jobs = self.scheduler.list_jobs().await;
        let caller = current_identity_id();
        let is_admin = (self.is_admin)();

        let job_list: Vec<serde_json::Value> = jobs
            .iter()
            .filter(|j| schedule_visible_to(j, caller.as_deref(), is_admin))
            .map(|j| {
                let mut summary = job_summary(j);
                if schedule_mutable_by(j, caller.as_deref(), is_admin) {
                    summary["spent_usd"] = serde_json::json!(self.scheduler.job_spend(&j.id));
                }
                summary
            })
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({
                "jobs": job_list,
                "count": job_list.len()
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    /// Change a job in place. Owner or admin only; what makes a recurring job
    /// cost more (a session of its own, a faster schedule over the
    /// confirmation threshold) is a cost the user approves first, as on
    /// create.
    async fn handle_update(&self, input: SchedulerInput, start: Instant) -> Result<ToolResult> {
        let elapsed = || start.elapsed().as_millis() as u64;
        let Some(job_id) = input.job_id.clone().filter(|id| !id.is_empty()) else {
            return Ok(ToolResult::failure(
                "Missing 'job_id' — which job to update?",
                elapsed(),
            ));
        };
        let job = match self.job_if_mutable(&job_id).await {
            Ok(job) => job,
            Err(denied) => return Ok(denied),
        };
        let schedule = match schedule_of(&input) {
            Ok(schedule) => schedule,
            Err(e) => return Ok(ToolResult::failure(e, elapsed())),
        };
        let edit = JobEdit {
            name: input.name,
            schedule,
            enabled: input.enabled,
            visibility: input.visibility,
            session: input.context,
            tools: input.tools,
            confirmed: input.confirmed == Some(true),
        };
        Ok(
            match self.scheduler.update_job(&job_id, edit, &editor()).await {
                Ok(Some(job)) => {
                    let mut summary = job_summary(&job);
                    summary["status"] = serde_json::json!("updated");
                    ToolResult::success(summary, elapsed())
                }
                Ok(None) => ToolResult::failure(NOT_YOURS, elapsed()),
                Err(EditRefused::NeedsConfirmation(cost)) => ToolResult::success(
                    needs_confirmation_payload("Update", &job.name, &cost),
                    elapsed(),
                ),
                Err(EditRefused::Invalid(why)) => ToolResult::failure(why, elapsed()),
            },
        )
    }

    async fn handle_remove(&self, input: SchedulerInput, start: Instant) -> Result<ToolResult> {
        let job_id = match input.job_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'job_id' — which job to remove?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // Owner gate: refuse (opaquely) before touching another user's job.
        if let Err(denied) = self.job_if_mutable(&job_id).await {
            return Ok(denied);
        }

        let removed = self.scheduler.remove_job(&job_id).await;

        if removed {
            Ok(ToolResult::success(
                serde_json::json!({
                    "job_id": job_id,
                    "status": "removed"
                }),
                start.elapsed().as_millis() as u64,
            ))
        } else {
            Ok(ToolResult::failure(
                format!("Job '{}' not found", job_id),
                start.elapsed().as_millis() as u64,
            ))
        }
    }

    async fn handle_run(&self, input: SchedulerInput, start: Instant) -> Result<ToolResult> {
        let job_id = match input.job_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'job_id' — which job to run?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // Owner gate: refuse (opaquely) before running another user's job.
        if let Err(denied) = self.job_if_mutable(&job_id).await {
            return Ok(denied);
        }

        match self.scheduler.run_job_now(&job_id).await {
            Some(record) => Ok(ToolResult::success(
                serde_json::json!({
                    "job_id": record.job_id,
                    "job_name": record.job_name,
                    "success": record.success,
                    "duration_ms": record.duration_ms,
                    "output": record.output,
                    "delivered": record.delivered,
                    "note": record.note,
                }),
                start.elapsed().as_millis() as u64,
            )),
            None => Ok(ToolResult::failure(
                format!("Job '{}' not found", job_id),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    /// Redirect a job's spend to another identity. Admin-only: billing is a
    /// budget decision, and an owner must not be able to charge someone else.
    async fn handle_set_bill_to(
        &self,
        input: SchedulerInput,
        start: Instant,
    ) -> Result<ToolResult> {
        let elapsed = || start.elapsed().as_millis() as u64;
        if !(self.is_admin)() {
            return Ok(ToolResult::failure(
                "scheduler set_bill_to requires the system admin role".to_string(),
                elapsed(),
            ));
        }
        let Some(job_id) = input.job_id.as_deref().filter(|s| !s.is_empty()) else {
            return Ok(ToolResult::failure(
                "set_bill_to needs `job_id`".to_string(),
                elapsed(),
            ));
        };
        let bill_to = input.bill_to.clone().filter(|s| !s.is_empty());
        let describe = bill_to
            .clone()
            .map(|b| format!("billed to `{b}`"))
            .unwrap_or_else(|| "billed to its run-as identity again".to_string());
        if self.scheduler.update_job_bill_to(job_id, bill_to).await {
            Ok(ToolResult::success(
                serde_json::Value::String(format!(
                    "Job {job_id} is now {describe}. Access is unchanged — it still runs \
                     as the same identity."
                )),
                elapsed(),
            ))
        } else {
            Ok(ToolResult::failure(
                format!("no job with id {job_id}"),
                elapsed(),
            ))
        }
    }

    async fn handle_history(&self, input: SchedulerInput, start: Instant) -> Result<ToolResult> {
        let caller = current_identity_id();
        let is_admin = (self.is_admin)();

        let records = if let Some(job_id) = input.job_id.as_deref().filter(|id| !id.is_empty()) {
            // Single-job history reveals that job's run details, so gate it like
            // a mutation: admin or owner only, opaque refusal otherwise.
            if let Err(denied) = self.job_if_mutable(job_id).await {
                return Ok(denied);
            }
            self.scheduler.job_run_history(job_id).await
        } else {
            // Cross-job history: keep only records whose underlying job the
            // caller may see. Records for jobs that no longer exist are dropped
            // for non-admins (fail-closed); admins keep everything.
            let all = self.scheduler.run_history().await;
            let mut visible = Vec::with_capacity(all.len());
            for r in all {
                let keep = if is_admin {
                    true
                } else {
                    match self.scheduler.get_job(&r.job_id).await {
                        Some(job) => schedule_visible_to(&job, caller.as_deref(), is_admin),
                        None => false,
                    }
                };
                if keep {
                    visible.push(r);
                }
            }
            visible
        };

        let history: Vec<serde_json::Value> = records
            .iter()
            .rev()
            .take(20)
            .map(|r| {
                serde_json::json!({
                    "job_id": r.job_id,
                    "job_name": r.job_name,
                    "started_at": r.started_at.to_rfc3339(),
                    "duration_ms": r.duration_ms,
                    "success": r.success,
                    "output": r.output,
                    "delivered": r.delivered,
                    "note": r.note,
                })
            })
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({
                "runs": history,
                "count": history.len()
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::never_admin;
    use crate::hub_core::schedules::VISIBILITY_GLOBAL;
    use crate::scheduler::Scheduler;
    use crate::serve::Compose as _;
    use workforce_agent::runner::CURRENT_USER_ID;

    // ── cross-user authorization tests (through the tool's execute()) ────────

    /// Build a live `Scheduler` over a throwaway `AgentService`. A real LLM key
    /// isn't needed at runtime here — `from_config` only requires one to *be
    /// configured* at build time — so a placeholder satisfies it. The gate-path
    /// ops we exercise (`list`/`remove`/`run`-refusal/`history`) touch only the
    /// in-memory job store, never the model.
    async fn test_scheduler() -> Arc<Scheduler> {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = workforce_common::settings::WorkforceConfig::default();
        cfg.hub.data_dir = tmp.path().to_string_lossy().to_string();
        cfg.memory.db_path = std::path::Path::new(&cfg.hub.data_dir)
            .join("memory.db")
            .to_string_lossy()
            .into_owned();
        cfg.llm.api_key = Some("test-key".into());
        std::mem::forget(tmp);
        let svc = crate::serve::AgentService::from_config(cfg, None)
            .await
            .expect("from_config should build with default config + tempdir data_dir");
        Arc::new(Scheduler::new(svc))
    }

    fn agent_turn_payload() -> JobPayload {
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

    /// Seed a job owned by `owner` with the given visibility and return its id.
    async fn seed_job(
        sched: &Scheduler,
        name: &str,
        owner: &str,
        visibility: Option<&str>,
        payload: JobPayload,
    ) -> String {
        sched
            .add_job_owned(
                name,
                None,
                Schedule::Every {
                    every_secs: 3600,
                    anchor: None,
                },
                payload,
                Some(owner.to_string()),
                visibility.map(str::to_string),
            )
            .await
            .expect("hourly job is above the interval floor")
    }

    async fn create(tool: &SchedulerTool, user: &str, input: serde_json::Value) -> ToolResult {
        CURRENT_USER_ID
            .scope(user.to_string(), tool.execute(input))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_below_the_interval_floor_is_rejected() {
        let sched = test_scheduler().await;
        let tool = SchedulerTool::new(Arc::clone(&sched));
        let out = create(
            &tool,
            "user-1",
            serde_json::json!({"action": "create", "message": "poll", "every_secs": 60}),
        )
        .await;
        assert!(!out.success);
        assert!(out.error.as_deref().unwrap_or_default().contains("minimum"));
        assert!(sched.list_jobs().await.is_empty());
    }

    #[tokio::test]
    async fn expensive_recurring_create_needs_user_confirmation() {
        let sched = test_scheduler().await;
        let tool = SchedulerTool::new(Arc::clone(&sched));
        // 288 runs/day at the $0.12 default estimate ≈ $35/day > the $5
        // threshold: no job, a structured estimate instead.
        let out = create(
            &tool,
            "user-1",
            serde_json::json!({"action": "create", "message": "poll", "every_secs": 300}),
        )
        .await;
        assert!(out.success);
        let body = out.output;
        assert_eq!(body["status"], "needs_confirmation");
        assert!(body["estimate"]["per_day_usd"].as_f64().unwrap() > 5.0);
        assert!(!body["alternatives"].as_array().unwrap().is_empty());
        assert!(sched.list_jobs().await.is_empty());

        // Explicit confirmation creates the job with the configured caps.
        let out = create(
            &tool,
            "user-1",
            serde_json::json!({"action": "create", "name": "poller", "message": "poll",
                               "every_secs": 300, "confirmed": true}),
        )
        .await;
        assert!(out.success, "{:?}", out.error);
        assert_eq!(out.output["status"], "created");
        let jobs = sched.list_jobs().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].max_runs, Some(100));
        assert_eq!(jobs[0].budget_usd, Some(25.0));
        assert!(jobs[0].expires_at.is_some());
    }

    #[tokio::test]
    async fn cheap_recurring_create_runs_isolated_without_confirmation() {
        let sched = test_scheduler().await;
        let tool = SchedulerTool::new(Arc::clone(&sched));
        // Daily at $0.12/run is far below the threshold: created directly.
        let out = create(
            &tool,
            "user-1",
            serde_json::json!({"action": "create", "name": "daily", "message": "report",
                               "every_secs": 86400}),
        )
        .await;
        assert!(out.success, "{:?}", out.error);
        assert_eq!(out.output["status"], "created");
        assert_eq!(out.output["context"], "isolated");
        let jobs = sched.list_jobs().await;
        let JobPayload::AgentTurn { ref channel, .. } = jobs[0].payload else {
            panic!("expected agent turn payload");
        };
        assert!(
            channel.is_none(),
            "isolated jobs must not inherit a session"
        );
    }

    #[tokio::test]
    async fn conversation_context_always_needs_confirmation() {
        let sched = test_scheduler().await;
        let tool = SchedulerTool::new(Arc::clone(&sched));
        let out = create(
            &tool,
            "user-1",
            serde_json::json!({"action": "create", "message": "watch", "every_secs": 86400,
                               "context": "conversation"}),
        )
        .await;
        assert!(out.success);
        assert_eq!(out.output["status"], "needs_confirmation");
        assert!(out.output["reason"]
            .as_str()
            .unwrap()
            .contains("conversation context"));
    }

    fn session_channel(job: &ScheduledJob) -> Option<&str> {
        match &job.payload {
            JobPayload::AgentTurn { channel, .. } => channel.as_deref(),
            _ => None,
        }
    }

    #[tokio::test]
    async fn a_job_created_with_only_a_delivery_target_runs_fresh() {
        let sched = test_scheduler().await;
        let tool = SchedulerTool::new(Arc::clone(&sched));
        // What a model sends when it means "post it here": the delivery DM
        // as `channel_id`, and again as the `channel` an older schema took
        // for a session key.
        let out = create(
            &tool,
            "user-1",
            serde_json::json!({"action": "create", "name": "daily", "message": "report",
                               "every_secs": 86400, "channel": "D123", "channel_id": "D123",
                               "thread_id": "17.5"}),
        )
        .await;
        assert!(out.success, "{:?}", out.error);
        assert_eq!(out.output["status"], "created", "no confirmation asked");
        assert_eq!(out.output["context"], "isolated");
        let job = &sched.list_jobs().await[0];
        assert_eq!(job.session_mode(), Some(SessionMode::Fresh));
        let JobPayload::AgentTurn {
            channel_id,
            thread_id,
            ..
        } = &job.payload
        else {
            panic!("expected agent turn payload");
        };
        assert_eq!(
            (channel_id.as_deref(), thread_id.as_deref()),
            (Some("D123"), Some("17.5"))
        );

        let named = create(
            &tool,
            "user-1",
            serde_json::json!({"action": "create", "name": "other", "message": "report",
                               "every_secs": 86400, "channel": "team-updates",
                               "channel_id": "D123"}),
        )
        .await;
        assert!(!named.success, "a caller cannot name a session");
        assert!(named.error.unwrap_or_default().contains("channel_id"));
        assert_eq!(sched.list_jobs().await.len(), 1);
    }

    #[tokio::test]
    async fn a_confirmed_conversation_job_is_pinned_to_a_session_of_its_own() {
        let sched = test_scheduler().await;
        let tool = SchedulerTool::new(Arc::clone(&sched));
        let confirmed = serde_json::json!({"action": "create", "message": "watch",
            "every_secs": 86400, "context": "conversation", "confirmed": true});
        // Two people ask from the same surface, whose session key they share.
        for user in ["user-1", "user-2"] {
            let out = workforce_agent::runner::CURRENT_SESSION_KEY
                .scope("web".to_string(), create(&tool, user, confirmed.clone()))
                .await;
            assert!(out.success, "{:?}", out.error);
            assert_eq!(out.output["context"], "conversation");
        }
        let jobs = sched.list_jobs().await;
        let pins: Vec<&str> = jobs.iter().filter_map(session_channel).collect();
        assert_eq!(pins.len(), 2);
        assert!(
            jobs.iter().all(|job| job.pinned_session().is_some()),
            "{pins:?}"
        );
        assert_ne!(pins[0], pins[1], "one person's job never joins another's");
    }

    /// A job `owner` has pinned.
    async fn seed_pinned(sched: &Scheduler, name: &str, owner: &str) -> String {
        let id = seed_job(sched, name, owner, None, agent_turn_payload()).await;
        let pin = JobEdit {
            session: Some(SessionMode::Pinned),
            confirmed: true,
            ..Default::default()
        };
        sched.update_job(&id, pin, owner).await.unwrap().unwrap();
        id
    }

    #[tokio::test]
    async fn the_owner_and_an_admin_may_edit_a_job_and_another_member_may_not() {
        let sched = test_scheduler().await;
        let to_fresh = |job: &str| serde_json::json!({"action": "update", "job_id": job, "context": "isolated"});
        let member = SchedulerTool::new(Arc::clone(&sched)).with_admin_policy(never_admin());
        let admin = SchedulerTool::new(Arc::clone(&sched));

        let theirs = seed_pinned(&sched, "b-job", "user-B").await;
        let before = sched.get_job(&theirs).await.unwrap();
        for edit in [
            serde_json::json!({"context": "isolated"}),
            serde_json::json!({"tools": []}),
            serde_json::json!({"tools": ["exec"]}),
            serde_json::json!({"visibility": "global"}),
            serde_json::json!({"enabled": false, "name": "mine"}),
        ] {
            let mut input = edit.clone();
            input["action"] = "update".into();
            input["job_id"] = theirs.clone().into();
            let refused = create(&member, "user-A", input).await;
            assert!(!refused.success, "{edit}");
            assert_eq!(refused.error.as_deref(), Some(NOT_YOURS), "{edit}");
        }
        let after = sched.get_job(&theirs).await.unwrap();
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            serde_json::to_value(&before).unwrap(),
            "nothing another member asked for was applied"
        );

        let by_owner = create(&member, "user-B", to_fresh(&theirs)).await;
        assert!(by_owner.success, "{:?}", by_owner.error);
        assert_eq!(by_owner.output["context"], "isolated");
        let job = sched.get_job(&theirs).await.unwrap();
        assert_eq!(job.session_mode(), Some(SessionMode::Fresh));

        let other = seed_pinned(&sched, "c-job", "user-C").await;
        let by_admin = create(&admin, "user-A", to_fresh(&other)).await;
        assert!(by_admin.success, "{:?}", by_admin.error);
        let job = sched.get_job(&other).await.unwrap();
        assert_eq!(job.session_mode(), Some(SessionMode::Fresh));
        assert_eq!(
            job.creator_identity_id.as_deref(),
            Some("user-C"),
            "an admin's edit leaves the job its owner's"
        );
    }

    #[tokio::test]
    async fn a_schedule_made_expensive_by_update_waits_for_the_users_confirmation() {
        let sched = test_scheduler().await;
        let tool = SchedulerTool::new(Arc::clone(&sched));
        let id = seed_job(&sched, "daily", "user-1", None, agent_turn_payload()).await;
        // 288 runs/day at the $0.12 default estimate ≈ $35/day > the $5
        // threshold, as on create.
        let faster = |confirmed: bool| {
            serde_json::json!({"action": "update", "job_id": id, "every_secs": 300,
                               "confirmed": confirmed})
        };

        let asked = create(&tool, "user-1", faster(false)).await;
        assert!(asked.success);
        assert_eq!(asked.output["status"], "needs_confirmation");
        assert!(asked.output["estimate"]["per_day_usd"].as_f64().unwrap() > 5.0);
        assert!(asked.output["instruction"]
            .as_str()
            .unwrap()
            .contains("Update the job"));
        let unchanged = sched.get_job(&id).await.unwrap();
        assert!(matches!(
            unchanged.schedule,
            Schedule::Every {
                every_secs: 3600,
                ..
            }
        ));

        let done = create(&tool, "user-1", faster(true)).await;
        assert!(done.success, "{:?}", done.error);
        let job = sched.get_job(&id).await.unwrap();
        assert!(matches!(
            job.schedule,
            Schedule::Every {
                every_secs: 300,
                ..
            }
        ));

        let slower = create(
            &tool,
            "user-1",
            serde_json::json!({"action": "update", "job_id": id, "every_secs": 600}),
        )
        .await;
        assert_eq!(
            slower.output["status"], "updated",
            "a cheaper schedule is not asked about"
        );
    }

    #[tokio::test]
    async fn pinning_an_existing_recurring_job_waits_for_the_users_confirmation() {
        let sched = test_scheduler().await;
        let tool = SchedulerTool::new(Arc::clone(&sched));
        let id = seed_job(&sched, "daily", "user-1", None, agent_turn_payload()).await;
        let pin = |confirmed: bool| {
            serde_json::json!({"action": "update", "job_id": id,
            "context": "conversation", "confirmed": confirmed})
        };

        let asked = create(&tool, "user-1", pin(false)).await;
        assert!(asked.success);
        assert_eq!(asked.output["status"], "needs_confirmation");
        assert!(asked.output["reason"]
            .as_str()
            .unwrap()
            .contains("every run replays"));
        assert_eq!(
            sched.get_job(&id).await.unwrap().session_mode(),
            Some(SessionMode::Fresh)
        );

        let done = create(&tool, "user-1", pin(true)).await;
        assert!(done.success, "{:?}", done.error);
        assert_eq!(done.output["status"], "updated");
        assert_eq!(done.output["context"], "conversation");
    }

    #[tokio::test]
    async fn update_edits_the_fields_the_rest_route_does_and_the_tool_list() {
        let sched = test_scheduler().await;
        let tool = SchedulerTool::new(Arc::clone(&sched));
        let id = seed_job(&sched, "daily", "user-1", None, agent_turn_payload()).await;

        let out = create(
            &tool,
            "user-1",
            serde_json::json!({"action": "update", "job_id": id, "name": "nightly",
                               "cron": "0 2 * * *", "enabled": false, "visibility": "global",
                               "tools": ["github"]}),
        )
        .await;
        assert!(out.success, "{:?}", out.error);
        assert_eq!(out.output["tools"], serde_json::json!(["github"]));
        let job = sched.get_job(&id).await.unwrap();
        assert_eq!((job.name.as_str(), job.enabled), ("nightly", false));
        assert_eq!(job.effective_visibility(), VISIBILITY_GLOBAL);
        assert!(matches!(job.schedule, Schedule::Cron { ref expr, .. } if expr == "0 2 * * *"));

        let too_fast = create(
            &tool,
            "user-1",
            serde_json::json!({"action": "update", "job_id": id, "every_secs": 60}),
        )
        .await;
        assert!(!too_fast.success);
        assert!(too_fast.error.unwrap_or_default().contains("minimum"));
    }

    #[tokio::test]
    async fn list_shows_each_jobs_context_run_count_and_spend() {
        let sched = test_scheduler().await;
        let tool = SchedulerTool::new(Arc::clone(&sched));
        let id = seed_job(&sched, "daily", "user-1", None, agent_turn_payload()).await;
        sched
            .service()
            .telemetry_store
            .record_cost(&workforce_common::persistence::CostRecord {
                agent_id: "a".to_string(),
                model: "m".to_string(),
                cost_usd: 1.25,
                task_id: Some(format!("scheduler:{id}:1")),
                ..Default::default()
            })
            .unwrap();

        let out = create(&tool, "user-1", serde_json::json!({"action": "list"})).await;
        assert!(out.success, "{:?}", out.error);
        let listed = &out.output["jobs"][0];
        assert_eq!(listed["context"], "isolated");
        assert_eq!(listed["run_count"], 0);
        assert_eq!(listed["spent_usd"], 1.25);
    }

    #[tokio::test]
    async fn explicit_caps_only_tighten_the_defaults() {
        let sched = test_scheduler().await;
        let tool = SchedulerTool::new(Arc::clone(&sched));
        let out = create(
            &tool,
            "user-1",
            serde_json::json!({"action": "create", "name": "tight", "message": "check",
                               "every_secs": 86400, "max_runs": 10, "expires_in": "1d"}),
        )
        .await;
        assert!(out.success, "{:?}", out.error);
        let jobs = sched.list_jobs().await;
        assert_eq!(jobs[0].max_runs, Some(10));

        let out = create(
            &tool,
            "user-1",
            serde_json::json!({"action": "create", "name": "loose", "message": "check",
                               "every_secs": 86400, "max_runs": 100000}),
        )
        .await;
        assert!(out.success, "{:?}", out.error);
        let jobs = sched.list_jobs().await;
        let loose = jobs.iter().find(|j| j.name == "loose").unwrap();
        assert_eq!(
            loose.max_runs,
            Some(100),
            "a param can never loosen the cap"
        );
    }

    #[tokio::test]
    async fn one_shots_skip_estimate_and_caps() {
        let sched = test_scheduler().await;
        let tool = SchedulerTool::new(Arc::clone(&sched));
        let out = create(
            &tool,
            "user-1",
            serde_json::json!({"action": "create", "name": "once", "message": "remind me",
                               "in_time": "2m"}),
        )
        .await;
        assert!(out.success, "{:?}", out.error);
        assert_eq!(out.output["status"], "created");
        let jobs = sched.list_jobs().await;
        assert!(jobs[0].max_runs.is_none());
        assert!(jobs[0].budget_usd.is_none());
        assert!(jobs[0].expires_at.is_none());
    }

    // A member (never_admin) must NOT be able to remove another user's job, and
    // the job must remain untouched after the refused call.
    #[tokio::test]
    async fn member_cannot_remove_another_users_job() {
        let sched = test_scheduler().await;
        let bid = seed_job(&sched, "b-job", "user-B", None, agent_turn_payload()).await;

        let tool = SchedulerTool::new(Arc::clone(&sched)).with_admin_policy(never_admin());
        let res = CURRENT_USER_ID
            .scope(
                "user-A".to_string(),
                tool.execute(serde_json::json!({"action": "remove", "job_id": bid})),
            )
            .await
            .unwrap();

        assert!(!res.success, "A must not remove B's job");
        assert_eq!(res.error.as_deref(), Some(NOT_YOURS));
        assert!(
            sched.get_job(&bid).await.is_some(),
            "B's job must be untouched"
        );
    }

    // The owner CAN remove their own job.
    #[tokio::test]
    async fn owner_can_remove_their_own_job() {
        let sched = test_scheduler().await;
        let bid = seed_job(&sched, "b-job", "user-B", None, agent_turn_payload()).await;

        let tool = SchedulerTool::new(Arc::clone(&sched)).with_admin_policy(never_admin());
        let res = CURRENT_USER_ID
            .scope(
                "user-B".to_string(),
                tool.execute(serde_json::json!({"action": "remove", "job_id": bid})),
            )
            .await
            .unwrap();

        assert!(res.success, "owner must remove their own job");
        assert!(sched.get_job(&bid).await.is_none(), "job should be gone");
    }

    // An admin CAN remove any user's job (status-aware gate proven via the
    // always_admin lever).
    #[tokio::test]
    async fn admin_can_remove_any_users_job() {
        let sched = test_scheduler().await;
        let bid = seed_job(&sched, "b-job", "user-B", None, agent_turn_payload()).await;

        let tool = SchedulerTool::new(Arc::clone(&sched)).with_admin_policy(always_admin());
        let res = CURRENT_USER_ID
            .scope(
                "user-A".to_string(),
                tool.execute(serde_json::json!({"action": "remove", "job_id": bid})),
            )
            .await
            .unwrap();

        assert!(res.success, "admin must remove any job");
        assert!(sched.get_job(&bid).await.is_none());
    }

    // `run` on another user's job is refused before it executes; the job stays.
    #[tokio::test]
    async fn member_cannot_run_another_users_job() {
        let sched = test_scheduler().await;
        let bid = seed_job(&sched, "b-job", "user-B", None, agent_turn_payload()).await;

        let tool = SchedulerTool::new(Arc::clone(&sched)).with_admin_policy(never_admin());
        let res = CURRENT_USER_ID
            .scope(
                "user-A".to_string(),
                tool.execute(serde_json::json!({"action": "run", "job_id": bid})),
            )
            .await
            .unwrap();

        assert!(!res.success, "A must not run B's job");
        assert_eq!(res.error.as_deref(), Some(NOT_YOURS));
        assert!(sched.get_job(&bid).await.is_some());
    }

    // `history` for a specific job is gated like a mutation (opaque refusal).
    #[tokio::test]
    async fn member_cannot_read_another_users_job_history() {
        let sched = test_scheduler().await;
        let bid = seed_job(&sched, "b-job", "user-B", None, agent_turn_payload()).await;

        let tool = SchedulerTool::new(Arc::clone(&sched)).with_admin_policy(never_admin());
        let res = CURRENT_USER_ID
            .scope(
                "user-A".to_string(),
                tool.execute(serde_json::json!({"action": "history", "job_id": bid})),
            )
            .await
            .unwrap();

        assert!(!res.success);
        assert_eq!(res.error.as_deref(), Some(NOT_YOURS));
    }

    // `list` hides another user's private job from a member.
    #[tokio::test]
    async fn member_list_excludes_another_users_private_job() {
        let sched = test_scheduler().await;
        seed_job(&sched, "a-job", "user-A", None, agent_turn_payload()).await;
        seed_job(&sched, "b-job", "user-B", None, agent_turn_payload()).await;

        let tool = SchedulerTool::new(Arc::clone(&sched)).with_admin_policy(never_admin());
        let res = CURRENT_USER_ID
            .scope(
                "user-A".to_string(),
                tool.execute(serde_json::json!({"action": "list"})),
            )
            .await
            .unwrap();

        assert!(res.success);
        let jobs = res.output["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 1, "A sees only their own private job");
        assert_eq!(jobs[0]["name"], "a-job");
    }

    // The cross-job (no-id) `history` view is filtered to records whose
    // underlying job the caller may see — a member never sees another user's
    // private job's run records. Reminder runs fail delivery in tests (no Slack
    // surface) but are still recorded, which is all this filter test needs.
    #[tokio::test]
    async fn member_history_excludes_another_users_job_records() {
        let sched = test_scheduler().await;
        let aid = seed_job(
            &sched,
            "a-job",
            "user-A",
            None,
            JobPayload::Reminder {
                text: "a".into(),
                user_id: "user-A".into(),
            },
        )
        .await;
        let bid = seed_job(
            &sched,
            "b-job",
            "user-B",
            None,
            JobPayload::Reminder {
                text: "b".into(),
                user_id: "user-B".into(),
            },
        )
        .await;
        // Generate a run record for each job.
        sched.run_job_now(&aid).await;
        sched.run_job_now(&bid).await;

        let tool = SchedulerTool::new(Arc::clone(&sched)).with_admin_policy(never_admin());
        let res = CURRENT_USER_ID
            .scope(
                "user-A".to_string(),
                tool.execute(serde_json::json!({"action": "history"})),
            )
            .await
            .unwrap();

        assert!(res.success);
        let runs = res.output["runs"].as_array().unwrap();
        assert!(
            runs.iter().all(|r| r["job_id"] == aid),
            "A's history must contain only A's own job records, got: {runs:?}"
        );
    }

    // A global (non-reminder) job is VISIBLE to a non-owner member but NOT
    // mutable by them.
    #[tokio::test]
    async fn global_job_visible_but_not_mutable_by_non_owner() {
        let sched = test_scheduler().await;
        let gid = seed_job(
            &sched,
            "b-global",
            "user-B",
            Some(VISIBILITY_GLOBAL),
            agent_turn_payload(),
        )
        .await;

        let tool = SchedulerTool::new(Arc::clone(&sched)).with_admin_policy(never_admin());

        // Visible in A's list.
        let listed = CURRENT_USER_ID
            .scope(
                "user-A".to_string(),
                tool.execute(serde_json::json!({"action": "list"})),
            )
            .await
            .unwrap();
        let jobs = listed.output["jobs"].as_array().unwrap();
        assert!(
            jobs.iter().any(|j| j["name"] == "b-global"),
            "global job should be visible to A"
        );

        // But A cannot remove it.
        let removed = CURRENT_USER_ID
            .scope(
                "user-A".to_string(),
                tool.execute(serde_json::json!({"action": "remove", "job_id": gid})),
            )
            .await
            .unwrap();
        assert!(
            !removed.success,
            "A must not mutate a global job it doesn't own"
        );
        assert_eq!(removed.error.as_deref(), Some(NOT_YOURS));
        assert!(sched.get_job(&gid).await.is_some());
    }

    #[test]
    fn parse_relative_time_seconds() {
        assert_eq!(parse_relative_time("30s"), Some(Duration::seconds(30)));
    }

    #[test]
    fn parse_relative_time_minutes() {
        assert_eq!(parse_relative_time("2m"), Some(Duration::seconds(120)));
    }

    #[test]
    fn parse_relative_time_hours() {
        assert_eq!(parse_relative_time("1h"), Some(Duration::seconds(3600)));
    }

    #[test]
    fn parse_relative_time_days() {
        assert_eq!(parse_relative_time("1d"), Some(Duration::seconds(86400)));
    }

    #[test]
    fn parse_relative_time_combined() {
        assert_eq!(parse_relative_time("1h30m"), Some(Duration::seconds(5400)));
    }

    #[test]
    fn parse_relative_time_complex() {
        assert_eq!(
            parse_relative_time("2h15m30s"),
            Some(Duration::seconds(8130))
        );
    }

    #[test]
    fn parse_relative_time_bare_number() {
        assert_eq!(parse_relative_time("60"), Some(Duration::seconds(60)));
    }

    #[test]
    fn parse_relative_time_invalid() {
        assert_eq!(parse_relative_time(""), None);
        assert_eq!(parse_relative_time("abc"), None);
    }

    #[test]
    fn parse_relative_time_whitespace() {
        assert_eq!(parse_relative_time(" 5m "), Some(Duration::seconds(300)));
    }

    #[test]
    fn tool_definition() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string" }
            }
        });
        assert!(schema["properties"]["action"].is_object());
    }
}
