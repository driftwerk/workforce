//! A Claude Code turn that ended on an API error, read from what Claude Code
//! itself says of it rather than from the words of a reply.
//!
//! Verified on CLI 2.1.319, in a throwaway home with a stand-in API that
//! answers 429:
//! - A turn ended by an API error fires `StopFailure`, never `Stop`. Its
//!   payload has the usual `session_id`, `transcript_path`, `cwd`, plus
//!   `error` (one of `rate_limit`, `authentication_failed`, `billing_error`,
//!   `overloaded`, `server_error`, …), sometimes `error_details`, and
//!   `last_assistant_message`, the error as the terminal shows it.
//! - On a subscription sign-in at its plan's limit, `error` is `rate_limit`
//!   and the message reads "You've hit your 5-hour limit · resets 9am
//!   (Europe/London)". On an API key, or a refusal that isn't the plan's
//!   own (an organization's rate limit, a transient 429), `error` is
//!   `rate_limit` too but the message is "API Error: Request rejected
//!   (429) · …". So `rate_limit` alone is not a full plan.
//! - The transcript line for the error is written after the hook fires:
//!   `isApiErrorMessage`, `apiError: "usage_limit_reached"` (only for the
//!   plan's own limit) and `quotaLimits` / `apiErrorParams.rate_limit_info`
//!   with `rateLimitType` (`five_hour`, `seven_day`, …) and `resetsAt`, in
//!   epoch seconds. That is where the exact reset is read from.

use chrono::{DateTime, Utc};
use serde_json::Value;

/// The plan's own limit stopped a turn: which window, and when it resets
/// when Claude Code said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitHit {
    /// Claude Code's `rateLimitType`: `five_hour`, `seven_day`,
    /// `seven_day_opus`, … None when only the message was seen.
    pub window: Option<String>,
    pub resets_at: Option<DateTime<Utc>>,
}

/// How a turn ended that didn't end well, as Claude Code reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnFailure {
    /// On the plan's limit: the account is full until it resets.
    Limit(LimitHit),
    /// On another API error (`StopFailure`'s `error`): the turn failed, the
    /// account isn't full.
    Failed(String),
}

impl TurnFailure {
    pub fn limit(&self) -> Option<&LimitHit> {
        match self {
            TurnFailure::Limit(hit) => Some(hit),
            TurnFailure::Failed(_) => None,
        }
    }
}

/// What a `StopFailure` says: its `error`, and the message it showed.
pub fn of_stop_failure(error: &str, said: Option<&str>) -> TurnFailure {
    match error {
        "rate_limit" if said.is_some_and(says_plan_limit) => TurnFailure::Limit(LimitHit {
            window: None,
            resets_at: None,
        }),
        _ => TurnFailure::Failed(error.to_string()),
    }
}

/// Whether Claude Code's message says the plan's own limit was reached:
/// "You've hit your session limit · resets 9am", "You've hit your weekly
/// limit", "Claude AI usage limit reached". Not a 429 it reports as
/// "Request rejected", which can be another limit, or passing.
pub fn says_plan_limit(said: &str) -> bool {
    let s = said.trim().to_lowercase();
    let s = s.replace('’', "'");
    s.starts_with("you've hit your")
        || s.starts_with("you have hit your")
        || s.contains("usage limit reached")
        || s.contains("limit · resets")
}

/// The plan's limit as one transcript line says it: an API error line with
/// `apiError: "usage_limit_reached"`, or a `quotaLimits` refusal.
pub fn in_transcript_line(v: &Value) -> Option<LimitHit> {
    if v.get("isApiErrorMessage").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let quota = v.get("quotaLimits").filter(|q| q.is_object());
    let info = v.pointer("/apiErrorParams/rate_limit_info");
    let plan = v.get("apiError").and_then(Value::as_str) == Some("usage_limit_reached")
        || quota.is_some_and(|q| q.get("status").and_then(Value::as_str) == Some("rejected"));
    if !plan {
        return None;
    }
    let field = |k: &str| [info, quota].into_iter().flatten().find_map(|o| o.get(k));
    Some(LimitHit {
        window: field("rateLimitType")
            .and_then(Value::as_str)
            .map(str::to_string),
        resets_at: field("resetsAt")
            .and_then(Value::as_i64)
            .and_then(|s| DateTime::from_timestamp(s, 0)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_plans_own_limit_is_a_limit() {
        // The message a subscription sign-in shows at its plan's limit.
        assert_eq!(
            of_stop_failure(
                "rate_limit",
                Some("You've hit your 5-hour limit · resets 9am (Europe/London)")
            ),
            TurnFailure::Limit(LimitHit {
                window: None,
                resets_at: None
            })
        );
        assert!(
            of_stop_failure("rate_limit", Some("You’ve hit your weekly limit"))
                .limit()
                .is_some()
        );
        assert!(of_stop_failure(
            "rate_limit",
            Some("Claude AI usage limit reached|1792137600")
        )
        .limit()
        .is_some());
        // A 429 that isn't the plan's: the turn failed, the plan isn't full.
        assert_eq!(
            of_stop_failure(
                "rate_limit",
                Some("API Error: Request rejected (429) · This request would exceed your account's rate limit.")
            ),
            TurnFailure::Failed("rate_limit".into())
        );
        assert_eq!(
            of_stop_failure("rate_limit", None),
            TurnFailure::Failed("rate_limit".into())
        );
        assert_eq!(
            of_stop_failure("overloaded", Some("You've hit your limit")),
            TurnFailure::Failed("overloaded".into())
        );
    }

    #[test]
    fn the_transcript_says_which_window_and_when_it_resets() {
        // As Claude Code 2.1.319 wrote it, on a subscription sign-in.
        let line: Value = serde_json::from_str(
            r#"{"type":"assistant","isApiErrorMessage":true,"apiError":"usage_limit_reached",
            "apiErrorParams":{"rate_limit_info":{"status":"rejected","resetsAt":1792137600,
            "rateLimitType":"five_hour","overageStatus":"rejected",
            "overageDisabledReason":"org_level_disabled","isUsingOverage":false}},
            "quotaLimits":{"status":"rejected","resetsAt":1792137600,
            "unifiedRateLimitFallbackAvailable":false,"rateLimitType":"five_hour",
            "overageStatus":"rejected","overageDisabledReason":"org_level_disabled",
            "isUsingOverage":false},"error":"rate_limit","errorDetails":null,
            "message":{"content":[{"type":"text","text":"You've hit your session limit · resets 9am (Europe/London)"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            in_transcript_line(&line),
            Some(LimitHit {
                window: Some("five_hour".into()),
                resets_at: DateTime::from_timestamp(1_792_137_600, 0),
            })
        );
        // An API key's 429: no plan limit in it.
        let key: Value = serde_json::from_str(
            r#"{"type":"assistant","isApiErrorMessage":true,"apiError":null,
            "apiErrorParams":null,"quotaLimits":null,"error":"rate_limit","apiErrorStatus":429}"#,
        )
        .unwrap();
        assert_eq!(in_transcript_line(&key), None);
        // A reply, not an error.
        let reply: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"You've hit your usage limit"}]}}"#,
        )
        .unwrap();
        assert_eq!(in_transcript_line(&reply), None);
    }
}
