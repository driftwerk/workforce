use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use eyre::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use workforce_agent::runner::ToolWhitelist;
use workforce_common::heartbeat_check::{self, HeartbeatCheck};
use workforce_common::session::TurnPrincipal;
use workforce_common::settings::{HeartbeatDeliverMode, HeartbeatSettings};
use workforce_common::subscription::HEARTBEAT;

pub use crate::hub_core::heartbeats::{HeartbeatRecord, HeartbeatWakeHandle, WakeCommand};
use crate::memory::MemoryManager;
use crate::scheduler::isolated_run_channel;
use crate::serve::{AgentService, Route, Skip, TurnOutcome};

/// The id a legacy (HEARTBEAT.md) heartbeat runs under in place of a check's.
const LEGACY_CHECK_ID: &str = "legacy";

/// A previously reported issue, used for deduplication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportedIssue {
    pub first_reported: DateTime<Utc>,
    pub last_reported: DateTime<Utc>,
    pub report_count: u32,
    pub content_hash: String,
}

/// Persistent heartbeat state.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeartbeatState {
    pub last_checks: HashMap<String, i64>,
    pub last_heartbeat: Option<DateTime<Utc>>,
    /// Reported issues for deduplication.
    #[serde(default)]
    pub reported_issues: HashMap<String, ReportedIssue>,
}

impl HeartbeatState {
    pub fn load(workspace: &Path) -> Self {
        let path = workspace.join("memory").join("heartbeat-state.json");
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, workspace: &Path) -> Result<()> {
        let memory_dir = workspace.join("memory");
        std::fs::create_dir_all(&memory_dir)?;
        let path = memory_dir.join("heartbeat-state.json");
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn mark_checked(&mut self, check_name: &str) {
        self.last_checks
            .insert(check_name.to_string(), Utc::now().timestamp());
    }

    pub fn time_since_check(&self, check_name: &str) -> Option<i64> {
        self.last_checks
            .get(check_name)
            .map(|ts| Utc::now().timestamp() - ts)
    }
}

/// Retry state for a single check (not persisted).
#[derive(Debug, Clone, Default)]
struct RetryState {
    consecutive_failures: u32,
    last_failure: Option<DateTime<Utc>>,
    backoff_until: Option<DateTime<Utc>>,
}

/// Point the delivery of every check of `service`'s that `owner` made and
/// `retarget` moves elsewhere, and sign it again as theirs, so it goes on
/// running as its creator. A check whose signature no longer verifies is
/// left alone: it runs as nobody's already, and re-signing would adopt
/// someone's edit. Returns the checks changed.
pub(crate) fn retarget_checks(
    service: &AgentService,
    owner: &str,
    retarget: &dyn Fn(
        &workforce_common::delivery::DeliveryTarget,
    ) -> Option<workforce_common::delivery::DeliveryTarget>,
) -> Vec<String> {
    let Some(signer) = service.check_signer.as_deref() else {
        return Vec::new();
    };
    let checks = match heartbeat_check::list_heartbeat_checks(&service.workspace) {
        Ok(checks) => checks,
        Err(e) => {
            warn!(error = %e, "Heartbeat: checks could not be read to point them elsewhere");
            return Vec::new();
        }
    };
    checks
        .into_iter()
        .filter(|check| signer.creator(check).is_some_and(|c| c.to_string() == owner))
        .filter_map(|mut check| {
            check.meta.deliver_to = Some(check.meta.deliver_to.as_ref().and_then(retarget)?);
            crate::heartbeat_signature::stamp_creator(Some(signer), &mut check, Some(owner));
            match heartbeat_check::save_heartbeat_check(&service.workspace, &check) {
                Ok(()) => Some(check.id),
                Err(e) => {
                    warn!(check_id = %check.id, error = %e, "Heartbeat: a check could not be pointed elsewhere");
                    None
                }
            }
        })
        .collect()
}

/// The heartbeat runner -- manages periodic agent check-ins with external delivery.
pub struct HeartbeatRunner {
    /// What the agent runs with now: the install's `base`, its own
    /// `[heartbeat]` over it, re-read each minute.
    config: HeartbeatSettings,
    base: HeartbeatSettings,
    service: Arc<AgentService>,
    workspace: std::path::PathBuf,
    user_tz: Tz,
    /// Ring buffer of recent heartbeat results.
    history: Arc<RwLock<Vec<HeartbeatRecord>>>,
    /// Channel for triggering manual wake-ups.
    wake_rx: tokio::sync::mpsc::Receiver<WakeCommand>,
    /// When set, only fire heartbeats if this flag is true (leader election).
    leader: Option<Arc<AtomicBool>>,
    /// Per-check retry state (not persisted across restarts).
    retry_states: HashMap<String, RetryState>,
    /// Checks whose skip has been logged since this runner started.
    warned_skips: HashSet<String>,
}

impl HeartbeatRunner {
    /// Create a heartbeat runner with a wake handle for manual triggers.
    pub fn new(
        config: HeartbeatSettings,
        service: Arc<AgentService>,
        workspace: &Path,
        user_tz: Tz,
    ) -> (Self, HeartbeatWakeHandle) {
        let (wake_tx, wake_rx) = tokio::sync::mpsc::channel(8);
        let history = Arc::new(RwLock::new(Vec::new()));

        let handle = HeartbeatWakeHandle {
            wake_tx,
            history: Arc::clone(&history),
        };

        let runner = Self {
            base: config.clone(),
            config,
            service,
            workspace: workspace.to_path_buf(),
            user_tz,
            history,
            wake_rx,
            leader: None,
            retry_states: HashMap::new(),
            warned_skips: HashSet::new(),
        };

        (runner, handle)
    }

    /// Who `check` runs as: its creator while the hub's signature over the
    /// check verifies ([`crate::heartbeat_signature`]), unless that person is
    /// no longer an active member ([`Skip::PersonGone`]). Otherwise
    /// (unsigned, tampered, legacy or hand-written, or the legacy
    /// HEARTBEAT.md) the `heartbeat` sentinel, paid for by the person the
    /// agent's own work falls to. Never anyone else.
    fn principal(&self, check: Option<&HeartbeatCheck>) -> Result<TurnPrincipal, Skip> {
        let creator = check
            .zip(self.service.check_signer.as_deref())
            .and_then(|(check, signer)| signer.creator(check));
        match creator {
            Some(person) if self.service.has_left(person) => Err(Skip::PersonGone(person)),
            Some(person) => Ok(TurnPrincipal::run_as(Some(person.to_string()))),
            None => Ok(self.service.background_principal(Some(HEARTBEAT))),
        }
    }

    /// Run one heartbeat turn for `check` (`None`: the legacy HEARTBEAT.md)
    /// in its own session on `channel`, as [`Self::principal`] says, offered
    /// only `tools` when the heartbeat declares them. It still enters as the
    /// `heartbeat` source, so it is admitted, capped and recorded as
    /// automated work, and its prompt carries no one's private context.
    pub(crate) async fn turn(
        &self,
        channel: &str,
        prompt: &str,
        tools: Option<&[String]>,
        check: Option<&HeartbeatCheck>,
    ) -> eyre::Result<TurnOutcome> {
        let principal = match self.principal(check) {
            Ok(principal) => principal,
            Err(skip) => return Ok(TurnOutcome::Skipped(skip)),
        };
        let desk = check
            .and_then(|check| self.desk_of(check))
            .map(|(_, target)| workforce_common::channels::desk::DeskScope {
                target,
                attended: false,
            });
        workforce_agent::runner::with_desk(
            desk,
            self.service.run_background_unit(
                &principal,
                Route::Model(None),
                workforce_agent::runner::with_tool_whitelist(
                    tools.map(ToolWhitelist::for_own_session),
                    Box::pin(self.service.process_turn(
                        channel,
                        HEARTBEAT,
                        prompt,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        principal.run_as.as_deref(),
                        crate::serve::RunAuthor::Other,
                    )),
                ),
            ),
        )
        .await
    }

    /// The Desk conversation `check` was made in, and the person it is
    /// theirs to tell there: its creator, while its signature verifies.
    fn desk_of(
        &self,
        check: &HeartbeatCheck,
    ) -> Option<(String, workforce_common::channels::desk::DeskTarget)> {
        let target = check.meta.deliver_to.as_ref()?.desk_target()?.clone();
        let creator = self.service.check_signer.as_deref()?.creator(check)?;
        Some((creator.to_string(), target))
    }

    /// Tell `owner` at `target` what a check found, as a check-in. Whether
    /// it was saved there.
    async fn check_in(
        &self,
        owner: &str,
        target: &workforce_common::channels::desk::DeskTarget,
        found: &str,
    ) -> bool {
        match crate::desk_surface::deliver(
            &self.service,
            owner,
            target,
            crate::desk_surface::Delivery::text(
                workforce_common::channels::desk::DeliveryKind::CheckIn,
                found,
            ),
        )
        .await
        {
            Ok(_) => true,
            Err(e) => {
                tracing::error!(error = %e, "Heartbeat: a check-in to Desk failed");
                false
            }
        }
    }

    /// Record that a check did not run: in the history, and in the log once
    /// per check while this runner lives.
    async fn record_skip(&mut self, check_id: &str, check_name: &str, skip: &Skip) {
        if self.warned_skips.insert(check_id.to_string()) {
            warn!(check_id, reason = %skip, "Heartbeat check skipped");
        }
        self.push_history(HeartbeatRecord {
            timestamp: Utc::now(),
            response: String::new(),
            actionable: false,
            delivered: false,
            check_id: check_id.to_string(),
            check_name: check_name.to_string(),
            duration_ms: 0,
            error: None,
            skipped: Some(skip.to_string()),
        })
        .await;
    }

    /// Set the leader flag. When set, the heartbeat only fires when
    /// this instance is the cluster leader.
    pub fn set_leader_flag(&mut self, flag: Arc<AtomicBool>) {
        self.leader = Some(flag);
    }

    /// Run the heartbeat loop.
    ///
    /// Ticks every 60 seconds (or on manual wake), evaluates which checks are due,
    /// and executes them. Falls back to legacy HEARTBEAT.md behavior when no
    /// heartbeat check files exist.
    pub async fn run(mut self, cancel: tokio_util::sync::CancellationToken) {
        info!(
            interval_secs = self.config.interval_secs,
            active = %format!("{}-{}", self.config.active_start, self.config.active_end),
            channels = ?self.config.channels,
            deliver = ?self.config.deliver,
            "Heartbeat runner started"
        );

        loop {
            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {
                    self.refresh();
                    if !self.config.enabled {
                        continue;
                    }
                    if !self.is_active_hours() {
                        debug!("Skipping heartbeat outside active hours");
                        continue;
                    }

                    // DW-301: Skip during active user sessions
                    if self.config.skip_during_active_sessions
                        && !self.service.active_runners.read().await.is_empty()
                    {
                        debug!("Skipping heartbeat during active user sessions");
                        continue;
                    }

                    self.tick(None).await;
                }
                wake = self.wake_rx.recv() => {
                    match wake {
                        None => {
                            info!("Heartbeat wake channel closed, shutting down");
                            break;
                        }
                        Some(cmd) => {
                            info!("Manual heartbeat wake triggered");
                            match cmd {
                                WakeCommand::All => self.tick(None).await,
                                WakeCommand::Specific(id) => self.tick(Some(&id)).await,
                            }
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    info!("Heartbeat stopped (leadership lost or shutdown)");
                    break;
                }
            }
        }
    }

    /// The install's heartbeat with the agent's own `[heartbeat]` over it,
    /// as its `config.toml` says now. An unreadable file keeps what was.
    fn refresh(&mut self) {
        match workforce_common::settings::AgentWorkspaceConfig::load_with_text(&self.workspace) {
            Ok(own) => {
                let own = own.and_then(|(config, _)| config.heartbeat);
                self.config = self.base.for_agent(own.as_ref());
            }
            Err(e) => {
                debug!(error = %e, "Heartbeat: the agent's config could not be read; keeping the last")
            }
        }
    }

    /// Delivers what checks made outside Desk found: into the agent's Desk
    /// DM with its person where the Desk crew is on, as a check-in of the
    /// agent's own. Whether it went anywhere.
    async fn deliver(&self, message: String) -> bool {
        #[cfg(feature = "postgres")]
        if crate::desk_crew::heartbeat_found(&self.service, &message).await {
            return true;
        }
        #[cfg(not(feature = "postgres"))]
        let _ = message;
        false
    }

    /// Run one tick of the heartbeat system.
    ///
    /// If `only_check_id` is set, only that specific check is executed.
    /// Otherwise, all due checks are evaluated and executed.
    async fn tick(&mut self, only_check_id: Option<&str>) {
        // In multi-instance mode, only the leader fires heartbeats.
        if let Some(ref flag) = self.leader {
            if !flag.load(Ordering::Relaxed) {
                debug!("Skipping heartbeat (not leader)");
                return;
            }
        }

        let now = Utc::now();

        // Determine whether we use multi-check mode or legacy HEARTBEAT.md fallback
        let heartbeats_dir = heartbeat_check::heartbeats_dir(&self.workspace);
        let has_checks_dir = heartbeats_dir.exists()
            && std::fs::read_dir(&heartbeats_dir)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false);

        let ran = if has_checks_dir {
            self.tick_multi_check(now, only_check_id).await;
            true
        } else {
            // Legacy fallback: use HEARTBEAT.md
            self.beat_legacy().await
        };
        // Only a beat that ran counts as the last: legacy mode waits out
        // `interval_secs` from it, so a tick that found nothing due must not
        // put it off again (it did, every minute, and the list never ran
        // a second time).
        if !ran {
            return;
        }

        // Update persistent state
        let mut state = HeartbeatState::load(&self.workspace);
        state.last_heartbeat = Some(Utc::now());
        if let Err(e) = state.save(&self.workspace) {
            tracing::error!("Failed to save heartbeat state: {}", e);
        }
    }

    /// Multi-check tick: load checks, filter to due, execute, deliver.
    async fn tick_multi_check(&mut self, now: DateTime<Utc>, only_check_id: Option<&str>) {
        // 1. Load checks from disk
        let checks = match heartbeat_check::list_heartbeat_checks(&self.workspace) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to load heartbeat checks: {}", e);
                return;
            }
        };

        // 2. Filter to enabled only
        let enabled: Vec<&HeartbeatCheck> = checks.iter().filter(|c| c.meta.enabled).collect();

        if enabled.is_empty() {
            debug!("No enabled heartbeat checks found");
            return;
        }

        let mut state = HeartbeatState::load(&self.workspace);

        // 3. Determine which are due
        let due: Vec<&HeartbeatCheck> = if let Some(specific_id) = only_check_id {
            enabled
                .into_iter()
                .filter(|c| c.id == specific_id)
                .collect()
        } else {
            enabled
                .into_iter()
                .filter(|c| self.is_check_due(c, &state, &now))
                .collect()
        };

        if due.is_empty() {
            debug!("No heartbeat checks due this tick");
            return;
        }

        // 4. Already sorted by priority from list_heartbeat_checks; cap at max_checks_per_cycle
        let max = self.config.max_checks_per_cycle;
        let to_run: Vec<&HeartbeatCheck> = due.into_iter().take(max).collect();

        info!(count = to_run.len(), "Running heartbeat checks");

        // Determine if we're in quiet hours (delivery suppressed)
        let in_quiet_hours = self.is_quiet_hours(&now);

        // What every check found, for the daily notes, and what of it goes
        // out batched: a check made in Desk tells its own conversation.
        let mut actionable_results: Vec<(String, String)> = Vec::new();
        let mut batched: Vec<(String, String)> = Vec::new();

        // 5. Execute each check sequentially
        for check in &to_run {
            let start = std::time::Instant::now();

            let prompt = if check.meta.take_action {
                format!(
                    "VIGILANT CHECK: {}\n{}\n\nCurrent time (UTC): {}\n\nInstructions:\n- Investigate thoroughly using your tools\n- If actionable, TAKE ACTION immediately (fix, review, respond)\n- Summarise what you did in 1-2 sentences\n- If nothing needs attention, reply HEARTBEAT_OK",
                    check.meta.name,
                    check.body,
                    now.format("%Y-%m-%d %H:%M:%S"),
                )
            } else {
                format!(
                    "HEARTBEAT CHECK: {}\n{}\n\nCurrent time (UTC): {}\nIf nothing needs attention, reply HEARTBEAT_OK.",
                    check.meta.name,
                    check.body,
                    now.format("%Y-%m-%d %H:%M:%S"),
                )
            };

            let result = self
                .turn(
                    &isolated_run_channel(HEARTBEAT, &check.id, now),
                    &prompt,
                    check.meta.tools.as_deref(),
                    Some(check),
                )
                .await;

            let duration_ms = start.elapsed().as_millis() as u64;

            match result.map(TurnOutcome::delivered) {
                Ok(Err(skip)) => {
                    self.record_skip(&check.id, &check.meta.name, &skip).await;
                    state.mark_checked(&check.id);
                }
                Ok(Ok(response)) => {
                    let actionable = !response.contains("HEARTBEAT_OK");

                    // Deduplication (DW-304)
                    let is_duplicate = if actionable {
                        self.check_dedup(&check.id, &response, &mut state)
                    } else {
                        false
                    };

                    // Determine delivery
                    let should_deliver = if in_quiet_hours || is_duplicate {
                        false // DW-305: checks run but delivery suppressed during quiet hours or duplicates
                    } else {
                        match self.config.deliver {
                            HeartbeatDeliverMode::All => true,
                            HeartbeatDeliverMode::ActionableOnly => actionable,
                        }
                    };

                    if actionable && !is_duplicate {
                        let max_len = if check.meta.take_action { 2000 } else { 500 };
                        let truncated =
                            workforce_common::text::truncate_str(&response, max_len).to_string();
                        let told = match self.desk_of(check) {
                            Some((owner, target)) if should_deliver => {
                                self.check_in(
                                    &owner,
                                    &target,
                                    &format!("**{}**: {}", check.meta.name, truncated),
                                )
                                .await
                            }
                            _ => false,
                        };
                        if !told {
                            batched.push((check.meta.name.clone(), truncated.clone()));
                        }
                        actionable_results.push((check.meta.name.clone(), truncated));
                    }

                    if actionable {
                        info!(
                            check_id = %check.id,
                            duplicate = is_duplicate,
                            "Heartbeat response: {}",
                            workforce_common::text::truncate_str(&response, 200)
                        );
                    } else {
                        debug!(check_id = %check.id, "Heartbeat: nothing to report");
                    }

                    // Record in history
                    let record = HeartbeatRecord {
                        timestamp: now,
                        response: workforce_common::text::truncate_str(&response, 500).to_string(),
                        actionable,
                        delivered: should_deliver,
                        check_id: check.id.clone(),
                        check_name: check.meta.name.clone(),
                        duration_ms,
                        error: None,
                        skipped: None,
                    };
                    self.push_history(record).await;

                    // Clear retry state on success
                    self.retry_states.remove(&check.id);

                    // Mark checked
                    state.mark_checked(&check.id);
                }
                Err(e) => {
                    tracing::error!(check_id = %check.id, "Heartbeat check error: {}", e);

                    // Record error in history
                    let record = HeartbeatRecord {
                        timestamp: now,
                        response: String::new(),
                        actionable: false,
                        delivered: false,
                        check_id: check.id.clone(),
                        check_name: check.meta.name.clone(),
                        duration_ms,
                        error: Some(e.to_string()),
                        skipped: None,
                    };
                    self.push_history(record).await;

                    // Update retry state with backoff
                    self.record_failure(&check.id, &now);
                }
            }
        }

        // Save state (includes dedup data)
        if let Err(e) = state.save(&self.workspace) {
            tracing::error!("Failed to save heartbeat state: {}", e);
        }

        // DW-300: Log to daily notes
        if self.config.log_to_daily_notes && !actionable_results.is_empty() {
            let mgr = MemoryManager::new(&self.workspace);
            let findings: String = actionable_results
                .iter()
                .map(|(name, resp)| format!("- **{}**: {}", name, resp))
                .collect::<Vec<_>>()
                .join("\n");
            let summary = format!(
                "## Heartbeat Report ({})\n\n{}\n",
                Utc::now().format("%H:%M"),
                findings
            );
            let _ = mgr.append_daily(&summary);
        }

        // DW-299: Notification batching -- deliver all actionable results in one message
        if !batched.is_empty() && !in_quiet_hours {
            let should_deliver = match self.config.deliver {
                HeartbeatDeliverMode::All => true,
                HeartbeatDeliverMode::ActionableOnly => true, // batched is already filtered
            };

            if should_deliver {
                {
                    let count = batched.len();
                    let body: String = batched
                        .iter()
                        .map(|(name, resp)| {
                            format!(
                                "**{}**: {}",
                                name,
                                workforce_common::text::truncate_str(resp, 300)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    let message = format!(
                        "\u{1FAC0} Heartbeat Report ({} check(s))\n\n{}",
                        count, body
                    );
                    self.deliver(message).await;
                }
            }
        }
    }

    /// Check if a response is a duplicate of a previously reported issue.
    /// Returns true if the response should be suppressed.
    fn check_dedup(&self, check_id: &str, response: &str, state: &mut HeartbeatState) -> bool {
        let len = response.len();
        let slice = &response[..std::cmp::min(500, len)];
        let mut hasher = DefaultHasher::new();
        slice.hash(&mut hasher);
        let hash = format!("{:x}", hasher.finish());

        let now = Utc::now();
        let dedup_window = self.config.dedup_window_secs as i64;

        if let Some(existing) = state.reported_issues.get_mut(check_id) {
            if existing.content_hash == hash
                && (now - existing.last_reported).num_seconds() < dedup_window
            {
                // Duplicate within window -- suppress
                existing.last_reported = now;
                existing.report_count += 1;
                return true;
            }
            // Different content or window expired -- update
            existing.content_hash = hash;
            existing.last_reported = now;
            existing.report_count += 1;
            false
        } else {
            // First report for this check
            state.reported_issues.insert(
                check_id.to_string(),
                ReportedIssue {
                    first_reported: now,
                    last_reported: now,
                    report_count: 1,
                    content_hash: hash,
                },
            );
            false
        }
    }

    /// Record a failure for backoff calculation.
    fn record_failure(&mut self, check_id: &str, now: &DateTime<Utc>) {
        let entry = self.retry_states.entry(check_id.to_string()).or_default();
        entry.consecutive_failures += 1;
        entry.last_failure = Some(*now);

        // Calculate backoff: base_interval * multiplier^failures, capped at max
        let base = self.config.interval_secs as f64;
        let multiplier = self.config.backoff_multiplier;
        let max_backoff = self.config.max_backoff_secs;

        let backoff_secs =
            (base * multiplier.powi(entry.consecutive_failures as i32)).min(max_backoff as f64);

        entry.backoff_until = Some(*now + chrono::Duration::seconds(backoff_secs as i64));
    }

    /// Check if a specific heartbeat check is due to run.
    fn is_check_due(
        &self,
        check: &HeartbeatCheck,
        state: &HeartbeatState,
        now: &DateTime<Utc>,
    ) -> bool {
        let check_id = &check.id;

        // Check backoff
        if let Some(retry) = self.retry_states.get(check_id) {
            if let Some(backoff_until) = retry.backoff_until {
                if *now < backoff_until {
                    debug!(check_id = %check_id, "Check in backoff, skipping");
                    return false;
                }
            }
        }

        // Compute elapsed seconds since last run for this check (relative to `now`)
        let last_run_ts = state.last_checks.get(check_id).copied();
        let elapsed_secs = last_run_ts.map(|ts| now.timestamp() - ts);

        // Check cooldown
        if let Some(cooldown) = check.meta.cooldown_secs {
            if let Some(elapsed) = elapsed_secs {
                if (elapsed as u64) < cooldown {
                    debug!(check_id = %check_id, "Check in cooldown, skipping");
                    return false;
                }
            }
        }

        // Cron-based scheduling
        if let Some(ref cron_expr) = check.meta.schedule {
            let last_run = DateTime::from_timestamp(last_run_ts.unwrap_or(0), 0)
                .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap());

            // Find the next fire time after `last_run` and check if it's <= now
            if let Some(next_fire) = cron_next_run(
                cron_expr,
                check.meta.timezone.as_deref(),
                &last_run,
                self.user_tz,
            ) {
                return next_fire <= *now;
            }
            // If cron parse failed, fall through to interval
        }

        // Interval fallback
        if let Some(elapsed) = elapsed_secs {
            (elapsed as u64) >= self.config.interval_secs
        } else {
            true // Never run before
        }
    }

    /// Check if we're currently in quiet hours.
    fn is_quiet_hours(&self, now: &DateTime<Utc>) -> bool {
        match (&self.config.quiet_start, &self.config.quiet_end) {
            (Some(start), Some(end)) => {
                // Invert the logic: during quiet hours delivery is suppressed
                is_active_hours_at(*now, self.user_tz, start, end)
            }
            _ => false,
        }
    }

    /// Execute the legacy single-file heartbeat (HEARTBEAT.md).
    /// Whether it ran: not when HEARTBEAT.md has nothing in it or is not
    /// yet due.
    async fn beat_legacy(&mut self) -> bool {
        // Load HEARTBEAT.md for the prompt
        let heartbeat_md =
            workforce_common::config_files::read(&self.workspace.join("HEARTBEAT.md"))
                .ok()
                .flatten()
                .unwrap_or_default();

        if heartbeat_md.trim().is_empty()
            || heartbeat_md
                .lines()
                .all(|l| l.starts_with('#') || l.trim().is_empty())
        {
            debug!("HEARTBEAT.md empty or comments only, skipping");
            return false;
        }

        // Respect interval_secs for legacy mode
        let state = HeartbeatState::load(&self.workspace);
        if let Some(last) = state.last_heartbeat {
            let elapsed = (Utc::now() - last).num_seconds();
            if elapsed < self.config.interval_secs as i64 {
                debug!(
                    "Legacy heartbeat: not yet due ({}s < {}s)",
                    elapsed, self.config.interval_secs
                );
                return false;
            }
        }

        let prompt = self.config.prompt.clone().unwrap_or_else(|| {
            format!(
                "HEARTBEAT: Read HEARTBEAT.md if it exists (workspace context). \
                 Follow it strictly. Do not infer or repeat old tasks from prior chats. \
                 If nothing needs attention, reply HEARTBEAT_OK.\n\n\
                 HEARTBEAT.md contents:\n{}\n\n\
                 Current time (UTC): {}",
                heartbeat_md,
                Utc::now().format("%Y-%m-%d %H:%M:%S"),
            )
        });

        info!("Firing heartbeat (legacy mode)");
        match self
            .turn(
                &isolated_run_channel(HEARTBEAT, LEGACY_CHECK_ID, Utc::now()),
                &prompt,
                self.config.tools.as_deref(),
                None,
            )
            .await
            .map(TurnOutcome::delivered)
        {
            Ok(Err(skip)) => {
                self.record_skip("", "HEARTBEAT.md", &skip).await;
            }
            Ok(Ok(response)) => {
                let actionable = !response.contains("HEARTBEAT_OK");

                let in_quiet_hours = self.is_quiet_hours(&Utc::now());

                let should_deliver = if in_quiet_hours {
                    false
                } else {
                    match self.config.deliver {
                        HeartbeatDeliverMode::All => true,
                        HeartbeatDeliverMode::ActionableOnly => actionable,
                    }
                };

                let delivered = if should_deliver {
                    self.deliver(response.clone()).await
                } else {
                    debug!("Heartbeat: suppressed (HEARTBEAT_OK, actionable_only mode, or quiet hours)");
                    false
                };

                if actionable {
                    info!(
                        "Heartbeat response: {}",
                        workforce_common::text::truncate_str(&response, 200)
                    );
                } else {
                    debug!("Heartbeat: nothing to report");
                }

                // Record in history
                let record = HeartbeatRecord {
                    timestamp: Utc::now(),
                    response: workforce_common::text::truncate_str(&response, 500).to_string(),
                    actionable,
                    delivered,
                    check_id: String::new(),
                    check_name: "HEARTBEAT.md".to_string(),
                    duration_ms: 0,
                    error: None,
                    skipped: None,
                };
                self.push_history(record).await;
            }
            Err(e) => {
                tracing::error!("Heartbeat error: {}", e);
            }
        }
        true
    }

    /// Push a record into the history ring buffer.
    async fn push_history(&self, record: HeartbeatRecord) {
        let mut history = self.history.write().await;
        history.push(record);
        // Trim to configured size
        if history.len() > self.config.history_size {
            let excess = history.len() - self.config.history_size;
            history.drain(..excess);
        }
    }

    /// Check if we're in active hours, evaluated in the user timezone.
    fn is_active_hours(&self) -> bool {
        is_active_hours_at(
            Utc::now(),
            self.user_tz,
            &self.config.active_start,
            &self.config.active_end,
        )
    }
}

/// Compute the next occurrence of a cron expression after `now`.
///
/// The cron expression is evaluated in the timezone specified by `tz_name`,
/// falling back to `default_tz`. The result is converted back to UTC.
fn cron_next_run(
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

/// Parse a time string like "08:00" or "23:30" into hour and minute.
fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let hour: u32 = parts[0].parse().ok()?;
    let minute: u32 = parts[1].parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some((hour, minute))
}

/// Pure function: check whether a given UTC instant falls inside active hours
/// when evaluated in the supplied timezone.
///
/// Active hours are specified as "HH:MM" strings (e.g. "08:00" to "23:00").
/// Supports wrap-around midnight (e.g. "22:00" to "06:00").
fn is_active_hours_at(now_utc: DateTime<Utc>, tz: Tz, start: &str, end: &str) -> bool {
    let (start_h, start_m) = match parse_hhmm(start) {
        Some(v) => v,
        None => {
            warn!(
                start = start,
                "Invalid active_start time format, defaulting to active"
            );
            return true;
        }
    };
    let (end_h, end_m) = match parse_hhmm(end) {
        Some(v) => v,
        None => {
            warn!(
                end = end,
                "Invalid active_end time format, defaulting to active"
            );
            return true;
        }
    };

    let local = now_utc.with_timezone(&tz);
    let local_time = local.time();

    let start_time = NaiveTime::from_hms_opt(start_h, start_m, 0).unwrap();
    let end_time = NaiveTime::from_hms_opt(end_h, end_m, 0).unwrap();

    if start_time <= end_time {
        // Normal range: e.g. 08:00 to 23:00
        local_time >= start_time && local_time < end_time
    } else {
        // Wraps midnight: e.g. 22:00 to 06:00
        local_time >= start_time || local_time < end_time
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};
    use workforce_common::heartbeat_check::HeartbeatCheckMeta;

    #[test]
    fn heartbeat_state_lifecycle() {
        let dir = tempfile::tempdir().unwrap();

        let mut state = HeartbeatState::default();
        state.mark_checked("email");
        state.save(dir.path()).unwrap();

        let loaded = HeartbeatState::load(dir.path());
        assert!(loaded.last_checks.contains_key("email"));
        assert!(loaded.time_since_check("email").unwrap() < 2);
    }

    #[test]
    fn parse_hhmm_valid() {
        assert_eq!(parse_hhmm("08:00"), Some((8, 0)));
        assert_eq!(parse_hhmm("23:59"), Some((23, 59)));
        assert_eq!(parse_hhmm("00:00"), Some((0, 0)));
        assert_eq!(parse_hhmm("12:30"), Some((12, 30)));
    }

    #[test]
    fn parse_hhmm_invalid() {
        assert_eq!(parse_hhmm("25:00"), None);
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("abc"), None);
        assert_eq!(parse_hhmm(""), None);
        assert_eq!(parse_hhmm("12"), None);
    }

    #[test]
    fn active_hours_normal_range() {
        let london: Tz = "Europe/London".parse().unwrap();

        // 2026-01-15 (winter, GMT = UTC+0) at 12:00 UTC -> 12:00 GMT -> inside 08:00-23:00
        let at_noon = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        assert!(is_active_hours_at(at_noon, london, "08:00", "23:00"));

        // 03:00 UTC = 03:00 GMT -> outside
        let at_3am = Utc.with_ymd_and_hms(2026, 1, 15, 3, 0, 0).unwrap();
        assert!(!is_active_hours_at(at_3am, london, "08:00", "23:00"));
    }

    #[test]
    fn active_hours_wrap_midnight() {
        // Active 22:00-06:00 (night owl schedule)
        let at_1am = Utc.with_ymd_and_hms(2026, 6, 15, 1, 0, 0).unwrap();
        assert!(is_active_hours_at(at_1am, chrono_tz::UTC, "22:00", "06:00"));

        let at_23pm = Utc.with_ymd_and_hms(2026, 6, 15, 23, 0, 0).unwrap();
        assert!(is_active_hours_at(
            at_23pm,
            chrono_tz::UTC,
            "22:00",
            "06:00"
        ));

        let at_noon = Utc.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap();
        assert!(!is_active_hours_at(
            at_noon,
            chrono_tz::UTC,
            "22:00",
            "06:00"
        ));
    }

    #[test]
    fn active_hours_timezone_offset() {
        let ny: Tz = "America/New_York".parse().unwrap();

        // Active 08:00-23:00 in New York.
        // 12:00 UTC on summer day = 08:00 EDT -> inside (at boundary)
        let at_noon_utc = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        assert!(is_active_hours_at(at_noon_utc, ny, "08:00", "23:00"));

        // 04:00 UTC = 00:00 EDT -> outside
        let at_4am_utc = Utc.with_ymd_and_hms(2026, 7, 15, 4, 0, 0).unwrap();
        assert!(!is_active_hours_at(at_4am_utc, ny, "08:00", "23:00"));
    }

    #[test]
    fn active_hours_tokyo() {
        let tokyo: Tz = "Asia/Tokyo".parse().unwrap();

        // Active 08:00-23:00 JST (UTC+9).
        // 02:00 UTC = 11:00 JST -> inside
        let at_2am_utc = Utc.with_ymd_and_hms(2026, 3, 10, 2, 0, 0).unwrap();
        assert!(is_active_hours_at(at_2am_utc, tokyo, "08:00", "23:00"));

        // 16:00 UTC = 01:00 JST -> outside
        let at_4pm_utc = Utc.with_ymd_and_hms(2026, 3, 10, 16, 0, 0).unwrap();
        assert!(!is_active_hours_at(at_4pm_utc, tokyo, "08:00", "23:00"));
    }

    #[test]
    fn active_hours_minute_precision() {
        // Active 08:30-17:45 in UTC
        let at_830 = Utc.with_ymd_and_hms(2026, 6, 15, 8, 30, 0).unwrap();
        assert!(is_active_hours_at(at_830, chrono_tz::UTC, "08:30", "17:45"));

        let at_829 = Utc.with_ymd_and_hms(2026, 6, 15, 8, 29, 0).unwrap();
        assert!(!is_active_hours_at(
            at_829,
            chrono_tz::UTC,
            "08:30",
            "17:45"
        ));

        let at_1745 = Utc.with_ymd_and_hms(2026, 6, 15, 17, 45, 0).unwrap();
        assert!(!is_active_hours_at(
            at_1745,
            chrono_tz::UTC,
            "08:30",
            "17:45"
        ));
    }

    #[test]
    fn active_hours_invalid_format_defaults_active() {
        let at_noon = Utc.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap();
        assert!(is_active_hours_at(at_noon, chrono_tz::UTC, "bad", "23:00"));
        assert!(is_active_hours_at(at_noon, chrono_tz::UTC, "08:00", "bad"));
    }

    #[tokio::test]
    async fn wake_handle_channel_works() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WakeCommand>(8);
        let history = Arc::new(RwLock::new(Vec::new()));
        let handle = HeartbeatWakeHandle {
            wake_tx: tx,
            history: Arc::clone(&history),
        };

        assert!(handle.wake().await);
        let cmd = rx.recv().await;
        assert!(cmd.is_some());
        assert!(matches!(cmd.unwrap(), WakeCommand::All));
    }

    #[tokio::test]
    async fn wake_handle_specific_check() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WakeCommand>(8);
        let history = Arc::new(RwLock::new(Vec::new()));
        let handle = HeartbeatWakeHandle {
            wake_tx: tx,
            history: Arc::clone(&history),
        };

        assert!(handle.wake_check("my-check".to_string()).await);
        let cmd = rx.recv().await.unwrap();
        match cmd {
            WakeCommand::Specific(id) => assert_eq!(id, "my-check"),
            _ => panic!("Expected WakeCommand::Specific"),
        }
    }

    #[tokio::test]
    async fn history_ring_buffer() {
        let history = Arc::new(RwLock::new(Vec::new()));
        let max_size = 3;

        // Push 5 records
        for i in 0..5 {
            let mut h = history.write().await;
            h.push(HeartbeatRecord {
                timestamp: Utc::now(),
                response: format!("response {}", i),
                actionable: i % 2 == 0,
                delivered: false,
                check_id: String::new(),
                check_name: String::new(),
                duration_ms: 0,
                error: None,
                skipped: None,
            });
            if h.len() > max_size {
                let excess = h.len() - max_size;
                h.drain(..excess);
            }
        }

        let h = history.read().await;
        assert_eq!(h.len(), 3);
        assert_eq!(h[0].response, "response 2");
        assert_eq!(h[2].response, "response 4");
    }

    #[tokio::test]
    async fn history_handle_returns_records() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<WakeCommand>(8);
        let history = Arc::new(RwLock::new(vec![HeartbeatRecord {
            timestamp: Utc::now(),
            response: "test response".to_string(),
            actionable: true,
            delivered: true,
            check_id: "test".to_string(),
            check_name: "Test Check".to_string(),
            duration_ms: 100,
            error: None,
            skipped: None,
        }]));

        let handle = HeartbeatWakeHandle {
            wake_tx: tx,
            history: Arc::clone(&history),
        };

        let records = handle.history().await;
        assert_eq!(records.len(), 1);
        assert!(records[0].actionable);

        let last = handle.last().await;
        assert!(last.is_some());
        assert_eq!(last.unwrap().response, "test response");
    }

    // --- New tests for the multi-check system ---

    fn make_check(id: &str, priority: u32, schedule: Option<&str>) -> HeartbeatCheck {
        HeartbeatCheck {
            id: id.to_string(),
            meta: HeartbeatCheckMeta {
                name: format!("Check {}", id),
                enabled: true,
                schedule: schedule.map(|s| s.to_string()),
                priority,
                tags: vec![],
                timezone: None,
                cooldown_secs: None,
                take_action: false,
                tools: None,
                creator: None,
                creator_sig: None,
                deliver_to: None,
            },
            body: format!("Check body for {}", id),
        }
    }

    /// Create a test runner that won't segfault on drop.
    /// Returns ManuallyDrop to avoid dropping the null Arc<AgentService>.
    fn make_runner_for_test() -> std::mem::ManuallyDrop<HeartbeatRunner> {
        // We use a null pointer for AgentService since unit tests only call
        // helper methods that don't touch the service field.
        // ManuallyDrop prevents the null Arc from being dropped.
        std::mem::ManuallyDrop::new(HeartbeatRunner {
            config: HeartbeatSettings::default(),
            base: HeartbeatSettings::default(),
            service: unsafe { Arc::from_raw(std::ptr::null::<AgentService>()) },
            workspace: std::path::PathBuf::from("/tmp/test"),
            user_tz: chrono_tz::UTC,
            history: Arc::new(RwLock::new(Vec::new())),
            wake_rx: tokio::sync::mpsc::channel(1).1,
            leader: None,
            retry_states: HashMap::new(),
            warned_skips: HashSet::new(),
        })
    }

    #[test]
    fn is_check_due_with_cron() {
        let runner = make_runner_for_test();
        let check = make_check("test-cron", 1, Some("0 9 * * *"));

        // State: last run was at 2026-01-15 08:00 UTC
        let mut state = HeartbeatState::default();
        state.last_checks.insert(
            "test-cron".to_string(),
            Utc.with_ymd_and_hms(2026, 1, 15, 8, 0, 0)
                .unwrap()
                .timestamp(),
        );

        // Now is 2026-01-15 09:30 -> cron "0 9 * * *" fired at 09:00, which is after last run
        let now = Utc.with_ymd_and_hms(2026, 1, 15, 9, 30, 0).unwrap();
        assert!(runner.is_check_due(&check, &state, &now));

        // Now is 2026-01-15 08:30 -> cron next fire is 09:00, which is in the future
        let now_early = Utc.with_ymd_and_hms(2026, 1, 15, 8, 30, 0).unwrap();
        assert!(!runner.is_check_due(&check, &state, &now_early));
    }

    #[test]
    fn is_check_due_with_interval_fallback() {
        let mut runner = make_runner_for_test();
        runner.config.interval_secs = 600; // 10 minutes

        let check = make_check("test-interval", 1, None); // No cron

        let mut state = HeartbeatState::default();
        let base = Utc.with_ymd_and_hms(2026, 1, 15, 10, 0, 0).unwrap();
        state
            .last_checks
            .insert("test-interval".to_string(), base.timestamp());

        // 5 minutes later -> not due
        let now_5m = Utc.with_ymd_and_hms(2026, 1, 15, 10, 5, 0).unwrap();
        assert!(!runner.is_check_due(&check, &state, &now_5m));

        // 11 minutes later -> due
        let now_11m = Utc.with_ymd_and_hms(2026, 1, 15, 10, 11, 0).unwrap();
        assert!(runner.is_check_due(&check, &state, &now_11m));

        // Never run before -> due
        let empty_state = HeartbeatState::default();
        assert!(runner.is_check_due(&check, &empty_state, &base));
    }

    #[test]
    fn select_checks_by_priority() {
        // list_heartbeat_checks sorts by priority then ID
        let checks = vec![
            make_check("low", 10, None),
            make_check("high", 1, None),
            make_check("medium", 5, None),
        ];

        let mut sorted = checks.clone();
        sorted.sort_by(|a, b| a.meta.priority.cmp(&b.meta.priority).then(a.id.cmp(&b.id)));

        assert_eq!(sorted[0].id, "high");
        assert_eq!(sorted[1].id, "medium");
        assert_eq!(sorted[2].id, "low");
    }

    #[test]
    fn select_checks_max_per_cycle() {
        let checks = [
            make_check("a", 1, None),
            make_check("b", 2, None),
            make_check("c", 3, None),
            make_check("d", 4, None),
            make_check("e", 5, None),
        ];

        let max_per_cycle = 3;
        let selected: Vec<&HeartbeatCheck> = checks.iter().take(max_per_cycle).collect();

        assert_eq!(selected.len(), 3);
        assert_eq!(selected[0].id, "a");
        assert_eq!(selected[2].id, "c");
    }

    #[test]
    fn dedup_same_content() {
        let runner = make_runner_for_test();
        let mut state = HeartbeatState::default();

        let response = "Something is broken! Please fix the build.";

        // First call -- not a duplicate
        let dup1 = runner.check_dedup("test-check", response, &mut state);
        assert!(!dup1);
        assert!(state.reported_issues.contains_key("test-check"));

        // Second call with same content -- duplicate
        let dup2 = runner.check_dedup("test-check", response, &mut state);
        assert!(dup2);
        assert_eq!(state.reported_issues["test-check"].report_count, 2);
    }

    #[test]
    fn dedup_expired_window() {
        let mut runner = make_runner_for_test();
        runner.config.dedup_window_secs = 60; // 1 minute window

        let mut state = HeartbeatState::default();

        let response = "Something is broken!";

        // Insert a reported issue that was last reported 2 minutes ago
        let two_min_ago = Utc::now() - chrono::Duration::seconds(120);
        state.reported_issues.insert(
            "test-check".to_string(),
            ReportedIssue {
                first_reported: two_min_ago,
                last_reported: two_min_ago,
                report_count: 1,
                content_hash: {
                    let mut hasher = DefaultHasher::new();
                    response.hash(&mut hasher);
                    format!("{:x}", hasher.finish())
                },
            },
        );

        // Should NOT be duplicate because window expired
        let dup = runner.check_dedup("test-check", response, &mut state);
        assert!(!dup);
    }

    #[test]
    fn retry_backoff() {
        let mut runner = make_runner_for_test();
        runner.config.interval_secs = 60;
        runner.config.backoff_multiplier = 2.0;
        runner.config.max_backoff_secs = 7200;

        let now = Utc.with_ymd_and_hms(2026, 1, 15, 10, 0, 0).unwrap();

        // Record first failure
        runner.record_failure("test", &now);
        let retry = runner.retry_states.get("test").unwrap();
        assert_eq!(retry.consecutive_failures, 1);
        assert!(retry.backoff_until.is_some());

        // Backoff should be 60 * 2^1 = 120 seconds
        let expected_backoff = now + chrono::Duration::seconds(120);
        let actual_backoff = retry.backoff_until.unwrap();
        assert!(
            (actual_backoff - expected_backoff).num_seconds().abs() < 2,
            "Backoff should be ~120s, got {}s",
            (actual_backoff - now).num_seconds()
        );

        // Record second failure
        runner.record_failure("test", &now);
        let retry2 = runner.retry_states.get("test").unwrap();
        assert_eq!(retry2.consecutive_failures, 2);

        // Backoff should be 60 * 2^2 = 240 seconds
        let expected_backoff2 = now + chrono::Duration::seconds(240);
        let actual_backoff2 = retry2.backoff_until.unwrap();
        assert!(
            (actual_backoff2 - expected_backoff2).num_seconds().abs() < 2,
            "Backoff should be ~240s, got {}s",
            (actual_backoff2 - now).num_seconds()
        );
    }

    #[test]
    fn quiet_hours_suppresses_delivery() {
        let mut runner = make_runner_for_test();
        runner.config.quiet_start = Some("22:00".to_string());
        runner.config.quiet_end = Some("06:00".to_string());

        // 23:00 UTC -> in quiet hours
        let at_23 = Utc.with_ymd_and_hms(2026, 1, 15, 23, 0, 0).unwrap();
        assert!(runner.is_quiet_hours(&at_23));

        // 12:00 UTC -> not in quiet hours
        let at_12 = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        assert!(!runner.is_quiet_hours(&at_12));

        // No quiet hours configured -> never quiet
        runner.config.quiet_start = None;
        runner.config.quiet_end = None;
        assert!(!runner.is_quiet_hours(&at_23));
    }

    #[test]
    fn cron_next_run_basic() {
        let now = Utc.with_ymd_and_hms(2026, 1, 15, 8, 0, 0).unwrap();
        let next = cron_next_run("0 9 * * *", None, &now, chrono_tz::UTC);
        assert!(next.is_some());
        let next = next.unwrap();
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn cron_next_run_invalid() {
        let now = Utc::now();
        let next = cron_next_run("invalid cron", None, &now, chrono_tz::UTC);
        assert!(next.is_none());
    }

    #[test]
    fn reported_issue_serialization() {
        let mut state = HeartbeatState::default();
        state.reported_issues.insert(
            "test".to_string(),
            ReportedIssue {
                first_reported: Utc::now(),
                last_reported: Utc::now(),
                report_count: 1,
                content_hash: "abc123".to_string(),
            },
        );

        let json = serde_json::to_string(&state).unwrap();
        let loaded: HeartbeatState = serde_json::from_str(&json).unwrap();
        assert!(loaded.reported_issues.contains_key("test"));
        assert_eq!(loaded.reported_issues["test"].report_count, 1);
    }
}
