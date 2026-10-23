//! Daily briefing system.
//!
//! Registers a scheduled job that invokes the agent with a customisable
//! briefing prompt. Leverages the existing [`Scheduler`] infrastructure.

use eyre::Result;

use workforce_common::settings::BriefingConfig;

use crate::scheduler::{JobPayload, Schedule, Scheduler};

/// Build the briefing prompt from config (or use default).
pub fn build_briefing_prompt(config: &BriefingConfig) -> String {
    if let Some(ref custom) = config.prompt {
        return custom.clone();
    }

    "Good morning! Please prepare a daily briefing:\n\n\
     1. Read yesterday's daily notes for context on what happened.\n\
     2. List any open tasks or action items that need attention today.\n\
     3. Check for any pending Linear issues assigned to the team.\n\
     4. Review any open GitHub pull requests that need attention.\n\
     5. Note any upcoming deadlines or scheduled events.\n\n\
     Format your response as a concise Slack-friendly summary with sections. \
     Use bullet points and keep it scannable. If you have access to messaging, \
     deliver the briefing to the configured channel."
        .to_string()
}

/// Register the daily briefing job with the scheduler.
///
/// Returns the job ID on success.
pub async fn register_briefing_job(
    scheduler: &Scheduler,
    config: &BriefingConfig,
    workspace_tz: Option<&str>,
) -> Result<String> {
    let tz = config
        .timezone
        .clone()
        .or_else(|| workspace_tz.map(|s| s.to_string()));

    let schedule = Schedule::Cron {
        expr: config.cron.clone(),
        tz,
    };

    let prompt = build_briefing_prompt(config);

    let payload = JobPayload::AgentTurn {
        message: prompt,
        channel: None,
        channel_id: config.channel_id.clone(),
        thread_id: config.thread_id.clone(),
        model: config.model.clone(),
        user_id: None,
        max_rounds: None,
        // Scheduler delivers the briefing; the agent does not self-post.
        agent_owns_delivery: false,
        tools: None,
    };

    let job_id = scheduler
        .add_job_with_desc(
            "daily-briefing",
            Some("Automated daily briefing — summarises tasks, notes, and pending items"),
            schedule,
            payload,
        )
        .await
        .map_err(|e| eyre::eyre!("failed to register daily briefing: {e}"))?;

    Ok(job_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_disabled() {
        let config = BriefingConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.cron, "0 9 * * 1-5");
        assert!(config.timezone.is_none());
        assert!(config.channel_id.is_none());
        assert!(config.prompt.is_none());
        assert!(config.model.is_none());
    }

    #[test]
    fn default_prompt_has_expected_sections() {
        let config = BriefingConfig::default();
        let prompt = build_briefing_prompt(&config);
        assert!(prompt.contains("daily notes"));
        assert!(prompt.contains("open tasks"));
        assert!(prompt.contains("Linear"));
        assert!(prompt.contains("GitHub"));
        assert!(prompt.contains("Slack-friendly"));
    }

    #[test]
    fn custom_prompt_overrides_default() {
        let config = BriefingConfig {
            prompt: Some("Just say hello.".to_string()),
            ..Default::default()
        };
        let prompt = build_briefing_prompt(&config);
        assert_eq!(prompt, "Just say hello.");
    }
}
