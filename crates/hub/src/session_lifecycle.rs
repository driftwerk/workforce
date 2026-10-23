//! Session lifecycle management — daily resets, idle expiry, and archiving.
//!
//! Runs as a background task that periodically checks sessions and
//! cleans up expired/idle ones. Supports per-session-type and per-channel
//! reset policies via `SessionPoliciesConfig`.
//!

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use tracing::{debug, info, warn};

use workforce_common::session::SessionStore;
use workforce_common::settings::{SessionLifecycleConfig, SessionPoliciesConfig};

/// The session lifecycle manager.
pub struct SessionLifecycle {
    config: SessionLifecycleConfig,
    policies: SessionPoliciesConfig,
    sessions: Arc<dyn SessionStore>,
    data_dir: PathBuf,
    user_tz: Tz,
    /// Track per-reset-hour dates to avoid resetting multiple times per day.
    /// Key = reset_hour, Value = last date that hour was triggered.
    last_daily_reset_dates: tokio::sync::Mutex<HashMap<u32, NaiveDate>>,
}

impl SessionLifecycle {
    pub fn new(
        config: SessionLifecycleConfig,
        sessions: Arc<dyn SessionStore>,
        data_dir: PathBuf,
        user_tz: Tz,
    ) -> Self {
        Self {
            config,
            policies: SessionPoliciesConfig::default(),
            sessions,
            data_dir,
            user_tz,
            last_daily_reset_dates: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Create a lifecycle manager with session reset policies.
    pub fn with_policies(
        config: SessionLifecycleConfig,
        policies: SessionPoliciesConfig,
        sessions: Arc<dyn SessionStore>,
        data_dir: PathBuf,
        user_tz: Tz,
    ) -> Self {
        Self {
            config,
            policies,
            sessions,
            data_dir,
            user_tz,
            last_daily_reset_dates: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Run the lifecycle check loop as a background task.
    pub async fn run(self: Arc<Self>) {
        let interval = std::time::Duration::from_secs(self.config.check_interval_secs);
        info!(
            interval_secs = self.config.check_interval_secs,
            daily_reset_hour = ?self.config.daily_reset_hour,
            idle_expiry_minutes = ?self.config.idle_expiry_minutes,
            type_policies = self.policies.types.len(),
            channel_policies = self.policies.channels.len(),
            "Session lifecycle manager started"
        );

        loop {
            tokio::time::sleep(interval).await;

            if let Err(e) = self.check_lifecycle().await {
                warn!(error = %e, "Session lifecycle check failed");
            }
        }
    }

    /// Perform a single lifecycle check.
    ///
    /// Evaluates each session individually against its resolved reset policy
    /// (channel override → type default → global config).
    async fn check_lifecycle(&self) -> eyre::Result<()> {
        let now = Utc::now();
        let user_now = now.with_timezone(&self.user_tz);
        let current_hour = user_now.hour();
        let current_date = user_now.date_naive();

        let sessions = self.sessions.list(None, 1000)?;
        let mut daily_reset_count = 0u32;
        let mut idle_expired_count = 0u32;
        // Collect reset hours that triggered during this check so we can
        // update the dedup map after processing all sessions.
        let mut triggered_hours: HashMap<u32, NaiveDate> = HashMap::new();

        let last_resets = self.last_daily_reset_dates.lock().await;

        for session in &sessions {
            if session.message_count() == 0 {
                continue;
            }

            let policy = self.policies.resolve(
                &session.channel,
                session.context_type.as_ref(),
                &self.config,
            );

            // ── Daily Reset (per-session policy) ─────────────────────
            if policy.daily_reset {
                if let Some(reset_hour) = policy.reset_hour {
                    let already_reset = last_resets
                        .get(&reset_hour)
                        .is_some_and(|d| *d >= current_date);

                    if current_hour >= reset_hour && !already_reset {
                        debug!(
                            session_id = %session.id,
                            channel = %session.channel,
                            context_type = ?session.context_type,
                            reset_hour,
                            "Daily reset applies to session"
                        );

                        if self.config.archive_expired {
                            self.archive_session(session).await;
                        }

                        if let Err(e) = self.sessions.delete(&session.id) {
                            warn!(
                                session_id = %session.id,
                                error = %e,
                                "Failed to delete session during daily reset"
                            );
                        } else {
                            daily_reset_count += 1;
                        }

                        triggered_hours.insert(reset_hour, current_date);
                        continue; // Already deleted, skip idle check
                    }
                }
            }

            // ── Idle Expiry (per-session policy) ─────────────────────
            if policy.idle_expiry_minutes > 0 {
                let threshold = now - chrono::Duration::minutes(policy.idle_expiry_minutes as i64);
                if session.updated_at < threshold {
                    debug!(
                        session_id = %session.id,
                        channel = %session.channel,
                        idle_minutes = policy.idle_expiry_minutes,
                        idle_since = %session.updated_at,
                        "Idle expiry applies to session"
                    );

                    if self.config.archive_expired {
                        self.archive_session(session).await;
                    }

                    if let Err(e) = self.sessions.delete(&session.id) {
                        warn!(
                            session_id = %session.id,
                            error = %e,
                            "Failed to delete expired session"
                        );
                    } else {
                        idle_expired_count += 1;
                        info!(
                            session_id = %session.id,
                            idle_since = %session.updated_at,
                            "Session expired due to inactivity"
                        );
                    }
                }
            }
        }

        // Update the dedup map after processing all sessions for this tick.
        drop(last_resets);
        if !triggered_hours.is_empty() {
            let mut last_resets = self.last_daily_reset_dates.lock().await;
            last_resets.extend(triggered_hours);
        }

        if daily_reset_count > 0 {
            info!(count = daily_reset_count, "Daily reset: cleared sessions");
        }
        if idle_expired_count > 0 {
            info!(count = idle_expired_count, "Expired idle sessions");
        }

        Ok(())
    }

    /// Archive a session transcript to disk before clearing.
    async fn archive_session(&self, session: &workforce_common::session::Session) {
        let archive_dir = self.data_dir.join("archived_sessions");
        if let Err(e) = workforce_common::private_fs::ensure_private_dir(&archive_dir) {
            warn!(error = %e, "Failed to create archive directory");
            return;
        }

        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        let filename = format!("{}_{}.json", &session.id[..8], timestamp);
        let path = archive_dir.join(&filename);

        // Export session as JSON
        match self.sessions.export_session(&session.id) {
            Ok(export) => match serde_json::to_string_pretty(&export) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&path, json) {
                        warn!(
                            session_id = %session.id,
                            path = %path.display(),
                            error = %e,
                            "Failed to write session archive"
                        );
                    } else {
                        info!(
                            session_id = %session.id,
                            path = %path.display(),
                            "Session archived"
                        );
                    }
                }
                Err(e) => {
                    warn!(session_id = %session.id, error = %e, "Failed to serialize session for archive");
                }
            },
            Err(e) => {
                warn!(session_id = %session.id, error = %e, "Failed to export session for archive");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::session::{InMemorySessionStore, Session, SessionContextType};
    use workforce_common::settings::ResetPolicy;

    fn make_config(daily_hour: Option<u32>, idle_minutes: Option<u32>) -> SessionLifecycleConfig {
        SessionLifecycleConfig {
            daily_reset_hour: daily_hour,
            idle_expiry_minutes: idle_minutes,
            archive_expired: false, // Don't archive in tests
            check_interval_secs: 1,
        }
    }

    fn make_lifecycle(
        config: SessionLifecycleConfig,
    ) -> (Arc<SessionLifecycle>, Arc<dyn SessionStore>) {
        let sessions: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
        let lifecycle = Arc::new(SessionLifecycle::new(
            config,
            Arc::clone(&sessions),
            PathBuf::from("/tmp/test-lifecycle"),
            chrono_tz::UTC,
        ));
        (lifecycle, sessions)
    }

    fn make_lifecycle_with_policies(
        config: SessionLifecycleConfig,
        policies: SessionPoliciesConfig,
    ) -> (Arc<SessionLifecycle>, Arc<dyn SessionStore>) {
        let sessions: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
        let lifecycle = Arc::new(SessionLifecycle::with_policies(
            config,
            policies,
            Arc::clone(&sessions),
            PathBuf::from("/tmp/test-lifecycle"),
            chrono_tz::UTC,
        ));
        (lifecycle, sessions)
    }

    #[tokio::test]
    async fn idle_expiry_removes_old_sessions() {
        let config = make_config(None, Some(60)); // 60 min idle expiry
        let (lifecycle, sessions) = make_lifecycle(config);

        // Create a session with old timestamp
        let mut session = Session::new("test", "user1", "allan");
        session.add_user_message("hello", None);
        session.updated_at = Utc::now() - chrono::Duration::minutes(120); // 2 hours old
        sessions.save(&session).unwrap();

        // Create a fresh session
        let mut fresh = Session::new("test", "user2", "allan");
        fresh.add_user_message("hi", None);
        sessions.save(&fresh).unwrap();

        assert_eq!(sessions.list(None, 100).unwrap().len(), 2);

        // Run lifecycle check — should expire the old session
        lifecycle.check_lifecycle().await.unwrap();

        // Old session should be gone, fresh one remains
        let remaining = sessions.list(None, 100).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].owner, "user2");
    }

    #[tokio::test]
    async fn idle_expiry_skips_empty_sessions() {
        let config = make_config(None, Some(1));
        let (lifecycle, sessions) = make_lifecycle(config);

        // Create an empty session (no messages) with old timestamp
        let mut session = Session::new("test", "user1", "allan");
        session.updated_at = Utc::now() - chrono::Duration::hours(24);
        sessions.save(&session).unwrap();

        lifecycle.check_lifecycle().await.unwrap();

        // Empty session should still exist
        assert_eq!(sessions.list(None, 100).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn daily_reset_clears_sessions_at_configured_hour() {
        // Use hour 0 so the check always triggers (current hour >= 0)
        let config = make_config(Some(0), None);
        let (lifecycle, sessions) = make_lifecycle(config);

        let mut s1 = Session::new("test", "user1", "allan");
        s1.add_user_message("msg1", None);
        sessions.save(&s1).unwrap();

        let mut s2 = Session::new("test", "user2", "allan");
        s2.add_user_message("msg2", None);
        sessions.save(&s2).unwrap();

        assert_eq!(sessions.list(None, 100).unwrap().len(), 2);

        lifecycle.check_lifecycle().await.unwrap();

        assert_eq!(sessions.list(None, 100).unwrap().len(), 0);
    }

    #[test]
    fn lifecycle_config_defaults() {
        let config = SessionLifecycleConfig::default();
        assert!(config.daily_reset_hour.is_none());
        assert!(config.idle_expiry_minutes.is_none());
        assert!(config.archive_expired);
        assert_eq!(config.check_interval_secs, 300);
        assert!(!config.is_enabled());
    }

    #[test]
    fn lifecycle_config_enabled_check() {
        let mut config = SessionLifecycleConfig::default();
        assert!(!config.is_enabled());

        config.daily_reset_hour = Some(4);
        assert!(config.is_enabled());

        config.daily_reset_hour = None;
        config.idle_expiry_minutes = Some(60);
        assert!(config.is_enabled());

        config.idle_expiry_minutes = Some(0);
        assert!(!config.is_enabled());
    }

    // ─── Per-Channel Policy Tests ────────────────────────────────────────

    #[tokio::test]
    async fn channel_policy_disables_daily_reset_for_cli() {
        // Global: daily reset at hour 0
        let config = make_config(Some(0), None);

        // Channel override: CLI sessions should NOT reset daily
        let mut policies = SessionPoliciesConfig::default();
        policies.channels.insert(
            "cli".to_string(),
            ResetPolicy {
                daily_reset: Some(false),
                reset_hour: None,
                idle_expiry_minutes: None,
            },
        );

        let (lifecycle, sessions) = make_lifecycle_with_policies(config, policies);

        // Create a Slack session and a CLI session
        let mut slack_session = Session::new("slack", "user1", "allan");
        slack_session.add_user_message("hello from slack", None);
        sessions.save(&slack_session).unwrap();

        let mut cli_session = Session::new("cli", "user2", "allan");
        cli_session.add_user_message("hello from cli", None);
        sessions.save(&cli_session).unwrap();

        assert_eq!(sessions.list(None, 100).unwrap().len(), 2);

        lifecycle.check_lifecycle().await.unwrap();

        // Only the CLI session should survive (daily reset disabled for it)
        let remaining = sessions.list(None, 100).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].channel, "cli");
    }

    #[tokio::test]
    async fn channel_policy_overrides_idle_expiry() {
        // Global: 60 min idle expiry
        let config = make_config(None, Some(60));

        // Channel override: gateway sessions get 10 min idle expiry
        let mut policies = SessionPoliciesConfig::default();
        policies.channels.insert(
            "gateway".to_string(),
            ResetPolicy {
                daily_reset: None,
                reset_hour: None,
                idle_expiry_minutes: Some(10),
            },
        );

        let (lifecycle, sessions) = make_lifecycle_with_policies(config, policies);

        // Both sessions are 30 minutes old
        let mut slack_session = Session::new("slack", "user1", "allan");
        slack_session.add_user_message("hello", None);
        slack_session.updated_at = Utc::now() - chrono::Duration::minutes(30);
        sessions.save(&slack_session).unwrap();

        let mut gw_session = Session::new("gateway", "user2", "allan");
        gw_session.add_user_message("hello", None);
        gw_session.updated_at = Utc::now() - chrono::Duration::minutes(30);
        sessions.save(&gw_session).unwrap();

        lifecycle.check_lifecycle().await.unwrap();

        // Gateway session should be expired (30 > 10 min), Slack should survive (30 < 60 min)
        let remaining = sessions.list(None, 100).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].channel, "slack");
    }

    // ─── Per-Type Policy Tests ───────────────────────────────────────────

    #[tokio::test]
    async fn type_policy_different_idle_for_threads() {
        // Global: 120 min idle expiry
        let config = make_config(None, Some(120));

        // Type override: thread sessions expire after 15 min
        let mut policies = SessionPoliciesConfig::default();
        policies.types.insert(
            SessionContextType::Thread,
            ResetPolicy {
                daily_reset: None,
                reset_hour: None,
                idle_expiry_minutes: Some(15),
            },
        );

        let (lifecycle, sessions) = make_lifecycle_with_policies(config, policies);

        // DM session, 30 min old
        let mut dm = Session::new("slack", "user1", "allan");
        dm.context_type = Some(SessionContextType::Dm);
        dm.add_user_message("hello", None);
        dm.updated_at = Utc::now() - chrono::Duration::minutes(30);
        sessions.save(&dm).unwrap();

        // Thread session, 30 min old
        let mut thread = Session::new("slack", "user2", "allan");
        thread.context_type = Some(SessionContextType::Thread);
        thread.add_user_message("hello", None);
        thread.updated_at = Utc::now() - chrono::Duration::minutes(30);
        sessions.save(&thread).unwrap();

        lifecycle.check_lifecycle().await.unwrap();

        // Thread should be expired (30 > 15), DM should survive (30 < 120)
        let remaining = sessions.list(None, 100).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].context_type, Some(SessionContextType::Dm));
    }

    #[tokio::test]
    async fn channel_override_takes_precedence_over_type() {
        // Global: no daily reset, 60 min idle
        let config = make_config(None, Some(60));

        // Type: DM sessions idle at 30 min
        // Channel: CLI sessions idle at 120 min (overrides type for CLI+DM)
        let mut policies = SessionPoliciesConfig::default();
        policies.types.insert(
            SessionContextType::Dm,
            ResetPolicy {
                daily_reset: None,
                reset_hour: None,
                idle_expiry_minutes: Some(30),
            },
        );
        policies.channels.insert(
            "cli".to_string(),
            ResetPolicy {
                daily_reset: None,
                reset_hour: None,
                idle_expiry_minutes: Some(120),
            },
        );

        let (lifecycle, sessions) = make_lifecycle_with_policies(config, policies);

        // Slack DM, 45 min idle → type policy = 30 min → should expire
        let mut slack_dm = Session::new("slack", "user1", "allan");
        slack_dm.context_type = Some(SessionContextType::Dm);
        slack_dm.add_user_message("hello", None);
        slack_dm.updated_at = Utc::now() - chrono::Duration::minutes(45);
        sessions.save(&slack_dm).unwrap();

        // CLI DM, 45 min idle → channel policy = 120 min → should survive
        let mut cli_dm = Session::new("cli", "user2", "allan");
        cli_dm.context_type = Some(SessionContextType::Dm);
        cli_dm.add_user_message("hello", None);
        cli_dm.updated_at = Utc::now() - chrono::Duration::minutes(45);
        sessions.save(&cli_dm).unwrap();

        lifecycle.check_lifecycle().await.unwrap();

        let remaining = sessions.list(None, 100).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].channel, "cli");
    }

    #[tokio::test]
    async fn no_policies_falls_back_to_global() {
        // Global: 20 min idle expiry
        let config = make_config(None, Some(20));
        let policies = SessionPoliciesConfig::default(); // No overrides

        let (lifecycle, sessions) = make_lifecycle_with_policies(config, policies);

        // Session with no context type, 30 min old
        let mut session = Session::new("slack", "user1", "allan");
        session.add_user_message("hello", None);
        session.updated_at = Utc::now() - chrono::Duration::minutes(30);
        sessions.save(&session).unwrap();

        lifecycle.check_lifecycle().await.unwrap();

        // Should be expired (30 > 20)
        assert_eq!(sessions.list(None, 100).unwrap().len(), 0);
    }

    #[tokio::test]
    async fn channel_policy_disables_idle_expiry() {
        // Global: 30 min idle
        let config = make_config(None, Some(30));

        // Channel override: disable idle expiry for "persistent" channel
        let mut policies = SessionPoliciesConfig::default();
        policies.channels.insert(
            "persistent".to_string(),
            ResetPolicy {
                daily_reset: None,
                reset_hour: None,
                idle_expiry_minutes: Some(0), // Explicitly disabled
            },
        );

        let (lifecycle, sessions) = make_lifecycle_with_policies(config, policies);

        // Persistent session, 2 hours old
        let mut session = Session::new("persistent", "user1", "allan");
        session.add_user_message("hello", None);
        session.updated_at = Utc::now() - chrono::Duration::hours(2);
        sessions.save(&session).unwrap();

        lifecycle.check_lifecycle().await.unwrap();

        // Should NOT be expired (idle expiry disabled for this channel)
        assert_eq!(sessions.list(None, 100).unwrap().len(), 1);
    }
}
