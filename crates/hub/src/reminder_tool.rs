use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};

use crate::authz::{always_admin, IsAdminFn};
use crate::hub_core::schedules::{JobPayload, Schedule};
use crate::scheduler::{schedule_mutable_by, schedule_visible_to, Scheduler};
use crate::scheduler_tool::parse_relative_time;
use crate::user_config::current_identity_id;

/// Opaque refusal for cancelling a reminder the caller doesn't own. Identical
/// for not-found and not-owned so a member can't probe another user's reminder
/// IDs for existence.
const NOT_YOURS: &str = "Reminder not found or you can only manage your own reminders";

/// Dedicated reminder tool — creates direct DM reminders that bypass the agent entirely.
///
/// Unlike `SchedulerTool` (which creates `AgentTurn` jobs that wake the agent),
/// this tool creates `Reminder` jobs that the scheduler delivers directly as
/// Slack DMs. No agent involvement means 100% reliable delivery.
pub struct ReminderTool {
    scheduler: Arc<Scheduler>,
    /// Owner-scope admin gate. Reminders are always private to their owner, so
    /// for everyone but an admin both list and cancel are owner-only. Defaults
    /// to `always_admin` for single-tenant/CLI use and existing tests;
    /// `serve.rs` threads the real identity-backed policy.
    is_admin: IsAdminFn,
}

impl ReminderTool {
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
}

#[derive(Deserialize)]
struct ReminderInput {
    action: String,
    /// The reminder message text.
    #[serde(default)]
    message: Option<String>,
    /// Human-readable name for the reminder.
    #[serde(default)]
    name: Option<String>,
    /// Relative time: "2m", "1h", "30s", "1d", "1h30m"
    #[serde(default)]
    in_time: Option<String>,
    /// Absolute ISO 8601 datetime for one-shot reminder.
    #[serde(default)]
    at: Option<String>,
    /// Recurring interval: "2h", "30m", "1d"
    #[serde(default)]
    every: Option<String>,
    /// Cron expression for recurring schedule.
    #[serde(default)]
    cron: Option<String>,
    /// IANA timezone for cron expression, e.g. "Europe/London".
    #[serde(default)]
    timezone: Option<String>,
    /// Reminder/job ID (for cancel action).
    #[serde(default)]
    job_id: Option<String>,
}

#[async_trait]
impl Tool for ReminderTool {
    fn name(&self) -> &str {
        "reminder"
    }

    fn description(&self) -> &str {
        "Set, list, or cancel personal reminders"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["set", "list", "cancel"],
                    "description": "Action to perform"
                },
                "message": {
                    "type": "string",
                    "description": "The reminder message text (for set)"
                },
                "name": {
                    "type": "string",
                    "description": "Optional human-readable name for the reminder (for set)"
                },
                "in_time": {
                    "type": "string",
                    "description": "Relative time from now"
                },
                "at": {
                    "type": "string",
                    "description": "Absolute ISO 8601 datetime for one-shot reminder (for set)"
                },
                "every": {
                    "type": "string",
                    "description": "Recurring interval: '30m', '2h', '1d' (for set, recurring)"
                },
                "cron": {
                    "type": "string",
                    "description": "Cron expression for recurring schedule"
                },
                "timezone": {
                    "type": "string",
                    "description": "IANA timezone (default: UTC)"
                },
                "job_id": {
                    "type": "string",
                    "description": "Reminder ID to cancel (for cancel)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: ReminderInput = serde_json::from_value(input)
            .map_err(|e| eyre::eyre!("Invalid reminder input: {}", e))?;

        match input.action.as_str() {
            "set" => self.handle_set(input, start).await,
            "list" => self.handle_list(start).await,
            "cancel" => self.handle_cancel(input, start).await,
            other => Ok(ToolResult::failure(
                format!("Unknown action '{}'. Use: set, list, cancel", other),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

impl ReminderTool {
    async fn handle_set(&self, input: ReminderInput, start: Instant) -> Result<ToolResult> {
        let message = match input.message {
            Some(m) if !m.is_empty() => m,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'message' — what should the reminder say?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // Auto-capture user_id from the current agent context.
        let user_id = workforce_agent::runner::CURRENT_USER_ID
            .try_with(|u| u.clone())
            .ok()
            .filter(|u| !u.is_empty());

        let user_id = match user_id {
            Some(uid) => uid,
            None => {
                return Ok(ToolResult::failure(
                    "Cannot determine your user ID. Reminders can only be set during a conversation.",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let name = input
            .name
            .unwrap_or_else(|| format!("reminder-{}", Utc::now().timestamp()));

        // Determine schedule — priority: in_time > at > every > cron
        let schedule = if let Some(ref in_time) = input.in_time {
            match parse_relative_time(in_time) {
                Some(duration) => Schedule::At {
                    at: Utc::now() + duration,
                },
                None => {
                    return Ok(ToolResult::failure(
                        format!(
                            "Could not parse relative time '{}'. Use format like '5m', '1h', '2h30m', '1d'",
                            in_time
                        ),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else if let Some(ref at_str) = input.at {
            match at_str.parse::<chrono::DateTime<Utc>>() {
                Ok(at) => Schedule::At { at },
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Could not parse datetime '{}': {}", at_str, e),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else if let Some(ref every) = input.every {
            match parse_relative_time(every) {
                Some(duration) => Schedule::Every {
                    every_secs: duration.num_seconds() as u64,
                    anchor: None,
                },
                None => {
                    return Ok(ToolResult::failure(
                        format!(
                            "Could not parse interval '{}'. Use format like '30m', '2h', '1d'",
                            every
                        ),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else if let Some(ref cron_expr) = input.cron {
            Schedule::Cron {
                expr: cron_expr.clone(),
                tz: input.timezone.clone(),
            }
        } else {
            return Ok(ToolResult::failure(
                "No schedule specified. Use 'in_time' (e.g. '5m'), 'at' (ISO datetime), 'every' (e.g. '2h'), or 'cron' (e.g. '0 8 * * *').",
                start.elapsed().as_millis() as u64,
            ));
        };

        let payload = JobPayload::Reminder {
            text: message.clone(),
            user_id: user_id.clone(),
        };

        let job_id = match self
            .scheduler
            .add_job_with_desc(&name, None, schedule.clone(), payload)
            .await
        {
            Ok(id) => id,
            Err(e) => {
                return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64));
            }
        };

        let schedule_desc = match &schedule {
            Schedule::At { at } => format!("at {}", at.to_rfc3339()),
            Schedule::Every { every_secs, .. } => format_interval(*every_secs),
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
                "schedule": schedule_desc,
                "user_id": user_id,
                "status": "created",
                "type": "reminder"
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    async fn handle_list(&self, start: Instant) -> Result<ToolResult> {
        let jobs = self.scheduler.list_jobs().await;
        let caller = current_identity_id();
        let is_admin = (self.is_admin)();

        // Filter to reminders the caller may see. Reminders are always private
        // to their owner, so `schedule_visible_to` reduces to owner-or-admin.
        let reminders: Vec<serde_json::Value> = jobs
            .iter()
            .filter(|j| j.is_reminder)
            .filter(|j| schedule_visible_to(j, caller.as_deref(), is_admin))
            .map(|j| {
                serde_json::json!({
                    "id": j.id,
                    "name": j.name,
                    "enabled": j.enabled,
                    "next_run": j.next_run.map(|t| t.to_rfc3339()),
                    "last_run": j.last_run.map(|t| t.to_rfc3339()),
                    "run_count": j.run_count,
                    "payload": match &j.payload {
                        JobPayload::Reminder { text, user_id } => serde_json::json!({
                            "message": text,
                            "user_id": user_id,
                        }),
                        _ => serde_json::json!(null),
                    },
                })
            })
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({
                "reminders": reminders,
                "count": reminders.len()
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    async fn handle_cancel(&self, input: ReminderInput, start: Instant) -> Result<ToolResult> {
        let job_id = match input.job_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'job_id' — which reminder to cancel? Use 'list' to see active reminders.",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // Owner gate FIRST — before any existence/type detail leaks. Same
        // opaque message for not-found and not-owned, so a member cannot probe
        // another user's reminder IDs by observing distinct errors.
        let caller = current_identity_id();
        let is_admin = (self.is_admin)();
        let job = match self.scheduler.get_job(&job_id).await {
            Some(j) if schedule_mutable_by(&j, caller.as_deref(), is_admin) => j,
            _ => {
                return Ok(ToolResult::failure(
                    NOT_YOURS,
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // Caller owns it (or is admin) — now enforce that it's actually a
        // reminder before removing.
        if job.is_reminder {
            self.scheduler.remove_job(&job_id).await;
            Ok(ToolResult::success(
                serde_json::json!({
                    "job_id": job_id,
                    "name": job.name,
                    "status": "cancelled"
                }),
                start.elapsed().as_millis() as u64,
            ))
        } else {
            Ok(ToolResult::failure(
                format!(
                    "Job '{}' is not a reminder. Use the scheduler tool to manage scheduled tasks.",
                    job_id
                ),
                start.elapsed().as_millis() as u64,
            ))
        }
    }
}

/// Format an interval in seconds as a human-readable string.
fn format_interval(secs: u64) -> String {
    if secs >= 86400 && secs.is_multiple_of(86400) {
        format!("every {}d", secs / 86400)
    } else if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("every {}h", secs / 3600)
    } else if secs >= 60 && secs.is_multiple_of(60) {
        format!("every {}m", secs / 60)
    } else {
        format!("every {}s", secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::{always_admin, never_admin};
    use crate::scheduler::Scheduler;
    use crate::serve::Compose as _;
    use std::sync::Arc;
    use workforce_agent::runner::CURRENT_USER_ID;

    // ── cross-user authorization tests (through the tool's execute()) ────────

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

    /// Seed a reminder owned by `owner` and return its id. Reminders carry the
    /// owner both as creator and as the `Reminder.user_id` recipient.
    async fn seed_reminder(sched: &Scheduler, name: &str, owner: &str) -> String {
        sched
            .add_job_owned(
                name,
                None,
                Schedule::Every {
                    every_secs: 3600,
                    anchor: None,
                },
                JobPayload::Reminder {
                    text: "ping".into(),
                    user_id: owner.to_string(),
                },
                Some(owner.to_string()),
                None,
            )
            .await
            .expect("hourly reminder is above the interval floor")
    }

    // A member must NOT cancel another user's reminder; it stays untouched and
    // the refusal is the same opaque message used for not-found.
    #[tokio::test]
    async fn member_cannot_cancel_another_users_reminder() {
        let sched = test_scheduler().await;
        let bid = seed_reminder(&sched, "b-reminder", "user-B").await;

        let tool = ReminderTool::new(Arc::clone(&sched)).with_admin_policy(never_admin());
        let res = CURRENT_USER_ID
            .scope(
                "user-A".to_string(),
                tool.execute(serde_json::json!({"action": "cancel", "job_id": bid})),
            )
            .await
            .unwrap();

        assert!(!res.success, "A must not cancel B's reminder");
        assert_eq!(res.error.as_deref(), Some(NOT_YOURS));
        assert!(
            sched.get_job(&bid).await.is_some(),
            "B's reminder must be untouched"
        );
    }

    // The owner CAN cancel their own reminder.
    #[tokio::test]
    async fn owner_can_cancel_their_own_reminder() {
        let sched = test_scheduler().await;
        let bid = seed_reminder(&sched, "b-reminder", "user-B").await;

        let tool = ReminderTool::new(Arc::clone(&sched)).with_admin_policy(never_admin());
        let res = CURRENT_USER_ID
            .scope(
                "user-B".to_string(),
                tool.execute(serde_json::json!({"action": "cancel", "job_id": bid})),
            )
            .await
            .unwrap();

        assert!(res.success, "owner must cancel their own reminder");
        assert!(sched.get_job(&bid).await.is_none());
    }

    // An admin CAN cancel any user's reminder.
    #[tokio::test]
    async fn admin_can_cancel_any_users_reminder() {
        let sched = test_scheduler().await;
        let bid = seed_reminder(&sched, "b-reminder", "user-B").await;

        let tool = ReminderTool::new(Arc::clone(&sched)).with_admin_policy(always_admin());
        let res = CURRENT_USER_ID
            .scope(
                "user-A".to_string(),
                tool.execute(serde_json::json!({"action": "cancel", "job_id": bid})),
            )
            .await
            .unwrap();

        assert!(res.success, "admin must cancel any reminder");
        assert!(sched.get_job(&bid).await.is_none());
    }

    // `list` shows a member only their own reminders.
    #[tokio::test]
    async fn member_list_excludes_another_users_reminder() {
        let sched = test_scheduler().await;
        seed_reminder(&sched, "a-reminder", "user-A").await;
        seed_reminder(&sched, "b-reminder", "user-B").await;

        let tool = ReminderTool::new(Arc::clone(&sched)).with_admin_policy(never_admin());
        let res = CURRENT_USER_ID
            .scope(
                "user-A".to_string(),
                tool.execute(serde_json::json!({"action": "list"})),
            )
            .await
            .unwrap();

        assert!(res.success);
        let reminders = res.output["reminders"].as_array().unwrap();
        assert_eq!(reminders.len(), 1, "A sees only their own reminder");
        assert_eq!(reminders[0]["name"], "a-reminder");
    }

    #[test]
    fn format_interval_days() {
        assert_eq!(format_interval(86400), "every 1d");
        assert_eq!(format_interval(172800), "every 2d");
    }

    #[test]
    fn format_interval_hours() {
        assert_eq!(format_interval(3600), "every 1h");
        assert_eq!(format_interval(7200), "every 2h");
    }

    #[test]
    fn format_interval_minutes() {
        assert_eq!(format_interval(60), "every 1m");
        assert_eq!(format_interval(1800), "every 30m");
    }

    #[test]
    fn format_interval_seconds() {
        assert_eq!(format_interval(45), "every 45s");
    }

    #[test]
    fn format_interval_mixed() {
        // 90 minutes = not evenly divisible by 3600, but by 60
        assert_eq!(format_interval(5400), "every 90m");
    }
}
