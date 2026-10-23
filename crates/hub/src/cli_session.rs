//! A CLI transcript's life across the turns that continue it: when to start
//! a fresh one rather than resume, shared by `/cc` threads and agent turns.

use chrono::{DateTime, Utc};

/// Default: start a fresh CLI transcript after this many completed turns.
/// Long resumed transcripts charge their full prefix on every reply;
/// rotating caps that prefix in exchange for losing prior context. Tune per
/// install via `[claude_chat] rotate_after_turns`.
pub(crate) const DEFAULT_ROTATE_AFTER_TURNS: u64 = 50;

/// Default: start a fresh CLI transcript if this many seconds have passed
/// since the last completed turn. Past the 5-minute prompt-cache TTL the
/// resumed prefix re-pays cache_creation, so idle rotation trades context
/// for cost. Tune per install via `[claude_chat] rotate_after_idle_mins`.
pub(crate) const DEFAULT_ROTATE_AFTER_IDLE_SECS: i64 = 4 * 3600;

/// Reason a session was rotated. Drives the Slack notice copy and structured
/// logging — does not change the rotation mechanics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RotationReason {
    TurnCount,
    Idle,
    /// The transcript's workspace is no longer on disk — reclaimed by the
    /// idle sweeper, or relocated because the project's repo set changed.
    /// Resuming would leave the model describing files that do not exist.
    WorkspaceMissing,
}

/// When a transcript is rotated: after `turns` completed turns, or `idle_secs`
/// after its last reply (`[claude_chat]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationPolicy {
    pub turns: u64,
    pub idle_secs: i64,
}

impl Default for RotationPolicy {
    fn default() -> Self {
        Self {
            turns: DEFAULT_ROTATE_AFTER_TURNS,
            idle_secs: DEFAULT_ROTATE_AFTER_IDLE_SECS,
        }
    }
}

impl RotationPolicy {
    /// The install's `[claude_chat]` thresholds, each defaulted when unset.
    /// They govern every CLI transcript: `/cc` threads and agent turns on
    /// Claude Code and Codex alike.
    pub fn of(config: &workforce_common::settings::ClaudeChatConfig) -> Self {
        let defaults = Self::default();
        Self {
            turns: config.rotate_after_turns.unwrap_or(defaults.turns),
            idle_secs: config
                .rotate_after_idle_mins
                .map_or(defaults.idle_secs, |mins| mins.saturating_mul(60)),
        }
    }

    /// Why a transcript that answered `turns` turns, the last at
    /// `last_reply_at`, is to be replaced now; `None` to resume it. Idle
    /// time counts only from a recorded reply.
    pub(crate) fn rotation(
        &self,
        turns: u64,
        last_reply_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Option<RotationReason> {
        let idle_secs = last_reply_at.map_or(0, |at| (now - at).num_seconds());
        match (turns >= self.turns, idle_secs >= self.idle_secs) {
            (true, _) => Some(RotationReason::TurnCount),
            (false, true) => Some(RotationReason::Idle),
            (false, false) => None,
        }
    }
}

/// Whether a resumed transcript has lost the workspace it describes: the
/// launcher made a fresh one for a thread that had already done work in the
/// last. A thread that never completed a turn may only have lost a
/// materialise race, and retries instead of being told its files are gone.
pub(crate) fn workspace_lost(fresh: bool, resumed_transcript: bool, completed_turns: u64) -> bool {
    fresh && resumed_transcript && completed_turns > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_is_lost_only_under_a_resumed_transcript_with_work_in_it() {
        assert!(workspace_lost(true, true, 3));
        assert!(
            !workspace_lost(false, true, 3),
            "found as the last turn left it"
        );
        assert!(
            !workspace_lost(true, false, 3),
            "a new transcript expects a new workspace"
        );
        assert!(
            !workspace_lost(true, true, 0),
            "nothing was ever done in it"
        );
    }

    #[test]
    fn rotation_decision_keeps_uuid_when_under_thresholds() {
        let now = chrono::Utc::now();
        let policy = RotationPolicy::default();
        assert!(policy.rotation(0, None, now).is_none());
        assert!(policy
            .rotation(
                DEFAULT_ROTATE_AFTER_TURNS - 1,
                Some(now - chrono::Duration::seconds(DEFAULT_ROTATE_AFTER_IDLE_SECS - 1)),
                now,
            )
            .is_none());
    }

    #[test]
    fn rotation_decision_rotates_at_turn_threshold() {
        let now = chrono::Utc::now();
        let reason = RotationPolicy::default()
            .rotation(DEFAULT_ROTATE_AFTER_TURNS, Some(now), now)
            .expect("expected rotation");
        assert!(matches!(reason, RotationReason::TurnCount));
    }

    #[test]
    fn rotation_decision_rotates_at_idle_threshold() {
        let now = chrono::Utc::now();
        let reason = RotationPolicy::default()
            .rotation(
                1,
                Some(now - chrono::Duration::seconds(DEFAULT_ROTATE_AFTER_IDLE_SECS)),
                now,
            )
            .expect("expected rotation");
        assert!(matches!(reason, RotationReason::Idle));
    }

    #[test]
    fn rotation_decision_idle_only_fires_with_recorded_reply() {
        // No prior reply (e.g. mid-first-turn) should never rotate on idle alone.
        let now = chrono::Utc::now();
        assert!(RotationPolicy::default().rotation(1, None, now).is_none());
    }

    #[test]
    fn rotation_decision_honours_overridden_thresholds() {
        // Per-install `[claude_chat]` overrides must reach the decision.
        let now = chrono::Utc::now();
        let policy = RotationPolicy {
            turns: 5,
            idle_secs: 600,
        };
        assert!(matches!(
            policy.rotation(5, Some(now), now),
            Some(RotationReason::TurnCount)
        ));
        assert!(policy.rotation(4, Some(now), now).is_none());
        assert!(matches!(
            policy.rotation(1, Some(now - chrono::Duration::seconds(600)), now),
            Some(RotationReason::Idle)
        ));
    }
}
