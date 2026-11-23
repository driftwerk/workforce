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

/// Default: start a boss's own conversation off its DM on a fresh transcript
/// once its context reached this many tokens. Each wake resumes it whole, so
/// its size is what every wake costs. `[claude_chat]
/// crew_rotate_after_context_tokens`.
pub(crate) const DEFAULT_CREW_ROTATE_AFTER_CONTEXT_TOKENS: u64 = 200_000;

/// Default: the same, as a share of the model's window, well before the CLI
/// compacts the transcript itself. `[claude_chat] crew_rotate_window_percent`.
pub(crate) const DEFAULT_CREW_ROTATE_WINDOW_PERCENT: u8 = 60;

/// Reason a session was rotated. Drives the Slack notice copy and structured
/// logging — does not change the rotation mechanics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RotationReason {
    TurnCount,
    Idle,
    /// A boss's own conversation grew past its cap ([`CrewCap`]).
    Size,
    /// The transcript's workspace is no longer on disk — reclaimed by the
    /// idle sweeper, or relocated because the project's repo set changed.
    /// Resuming would leave the model describing files that do not exist.
    WorkspaceMissing,
}

/// When a transcript is rotated: after `turns` completed turns, or `idle_secs`
/// after its last reply, and a boss's own conversation once it is as big as
/// `crew` allows (`[claude_chat]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationPolicy {
    pub turns: u64,
    pub idle_secs: i64,
    pub crew: CrewCap,
}

/// How big a boss's own conversation grows before it is replaced: `tokens`
/// (0, never), and no more than `window_percent` of its model's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrewCap {
    pub tokens: u64,
    pub window_percent: u8,
}

impl CrewCap {
    /// The most context a transcript on a model of `window` tokens keeps;
    /// `None` when it never rotates on size.
    pub(crate) fn limit(&self, window: u32) -> Option<u64> {
        let share = u64::from(window) * u64::from(self.window_percent.clamp(1, 100)) / 100;
        (self.tokens > 0).then(|| self.tokens.min(share))
    }
}

/// The window Claude Code runs `model` in, as the turn names it: a million
/// tokens only when it asks for one (`opus[1m]`), else 200 000, whatever
/// the model itself could take.
pub(crate) fn cli_window(model: &str) -> u32 {
    match model.trim().to_ascii_lowercase().ends_with("[1m]") {
        true => 1_000_000,
        false => 200_000,
    }
}

impl Default for RotationPolicy {
    fn default() -> Self {
        Self {
            turns: DEFAULT_ROTATE_AFTER_TURNS,
            idle_secs: DEFAULT_ROTATE_AFTER_IDLE_SECS,
            crew: CrewCap {
                tokens: DEFAULT_CREW_ROTATE_AFTER_CONTEXT_TOKENS,
                window_percent: DEFAULT_CREW_ROTATE_WINDOW_PERCENT,
            },
        }
    }
}

impl RotationPolicy {
    /// The install's `[claude_chat]` thresholds, each defaulted when unset.
    /// The turn and idle ones govern every CLI transcript: `/cc` threads and
    /// agent turns on Claude Code and Codex alike; the size one only a
    /// boss's own conversations.
    pub fn of(config: &workforce_common::settings::ClaudeChatConfig) -> Self {
        let defaults = Self::default();
        Self {
            turns: config.rotate_after_turns.unwrap_or(defaults.turns),
            idle_secs: config
                .rotate_after_idle_mins
                .map_or(defaults.idle_secs, |mins| mins.saturating_mul(60)),
            crew: CrewCap {
                tokens: config
                    .crew_rotate_after_context_tokens
                    .unwrap_or(defaults.crew.tokens),
                window_percent: config
                    .crew_rotate_window_percent
                    .unwrap_or(defaults.crew.window_percent),
            },
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

    /// [`Self::rotation`] for a boss's own conversation, whose last turn's
    /// context was `context_tokens` on a model of `window` tokens: first
    /// whether it outgrew [`Self::crew`].
    pub(crate) fn crew_rotation(
        &self,
        turns: u64,
        last_reply_at: Option<DateTime<Utc>>,
        context_tokens: u64,
        window: u32,
        now: DateTime<Utc>,
    ) -> Option<RotationReason> {
        self.crew
            .limit(window)
            .filter(|limit| context_tokens >= *limit)
            .map(|_| RotationReason::Size)
            .or_else(|| self.rotation(turns, last_reply_at, now))
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
            ..RotationPolicy::default()
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

    #[test]
    fn a_crew_conversation_rotates_on_size_first() {
        let now = chrono::Utc::now();
        let policy = RotationPolicy::default();
        assert_eq!(
            policy.crew_rotation(
                DEFAULT_ROTATE_AFTER_TURNS,
                Some(now),
                200_000,
                1_000_000,
                now
            ),
            Some(RotationReason::Size),
            "size is said before the turn count"
        );
        assert_eq!(
            policy.crew_rotation(1, Some(now), 199_999, 1_000_000, now),
            None
        );
        assert_eq!(
            policy.crew_rotation(DEFAULT_ROTATE_AFTER_TURNS, Some(now), 0, 1_000_000, now),
            Some(RotationReason::TurnCount),
            "an unknown size falls through to the other limits"
        );
    }

    #[test]
    fn the_crew_cap_is_the_lower_of_its_tokens_and_its_share_of_the_window() {
        let cap = RotationPolicy::default().crew;
        assert_eq!(cap.limit(1_000_000), Some(200_000));
        assert_eq!(cap.limit(200_000), Some(120_000));
        let off = CrewCap { tokens: 0, ..cap };
        assert_eq!(off.limit(200_000), None, "0 turns it off");
        let wild = CrewCap {
            window_percent: 250,
            ..cap
        };
        assert_eq!(wild.limit(100_000), Some(100_000), "kept within 100%");
    }

    #[test]
    fn claude_code_s_window_is_the_one_the_turn_asks_for() {
        assert_eq!(cli_window("opus[1m]"), 1_000_000);
        assert_eq!(cli_window("claude-opus-5[1M]"), 1_000_000);
        assert_eq!(
            cli_window("claude-opus-5"),
            200_000,
            "a 1M-capable model without [1m] runs in 200k"
        );
        assert_eq!(cli_window("sonnet"), 200_000);
        assert_eq!(cli_window(""), 200_000);
    }

    #[test]
    fn the_crew_keys_reach_the_policy() {
        let config = workforce_common::settings::ClaudeChatConfig {
            crew_rotate_after_context_tokens: Some(90_000),
            crew_rotate_window_percent: Some(40),
            ..Default::default()
        };
        let policy = RotationPolicy::of(&config);
        assert_eq!(
            policy.crew,
            CrewCap {
                tokens: 90_000,
                window_percent: 40
            }
        );
        assert_eq!(policy.turns, DEFAULT_ROTATE_AFTER_TURNS);
    }
}
