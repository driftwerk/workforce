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

/// What a limit window stops. Claude Code names the window it stopped on
/// (`rateLimitType`): `five_hour` and `seven_day` are the plan's own, which
/// stop every session on the account; `seven_day_opus` and
/// `seven_day_sonnet` stop that model's sessions only; anything else
/// (`overage`, extra usage, monthly credits, a spend cap) is pay-as-you-go
/// being used up, which stops no plan session. Keep in step with
/// `workforce_common::usage::claude_reach`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// The plan: every session on the account.
    Plan,
    /// One model's weekly window (`opus`, `sonnet`).
    Model(String),
    /// Extra usage or credits: no plan session.
    Credit,
}

/// What the window Claude Code names stops. None named (only the message
/// was seen) is the plan's, as before.
pub fn reach_of(window: Option<&str>) -> Reach {
    match window {
        None | Some("five_hour" | "seven_day") => Reach::Plan,
        Some(w) => match w.strip_prefix("seven_day_") {
            Some(model) if !model.is_empty() => Reach::Model(model.to_lowercase()),
            _ => Reach::Credit,
        },
    }
}

/// The window a limit message names when it is not the plan's own:
/// "You're out of extra usage · resets Dec 1" (credits), "You've hit your
/// Opus limit" (that model's weekly window). None for the plan's own
/// wording ("session limit", "weekly limit", "5-hour limit").
pub fn window_in_words(said: &str) -> Option<&'static str> {
    let s = said.trim().to_lowercase().replace('’', "'");
    if says_extra_usage(&s) {
        Some("overage")
    } else if s.contains("opus limit") || s.contains("opus weekly") {
        Some("seven_day_opus")
    } else if s.contains("sonnet limit") || s.contains("sonnet weekly") {
        Some("seven_day_sonnet")
    } else {
        None
    }
}

/// Whether a message says extra usage (pay-as-you-go credits) is spent.
pub fn says_extra_usage(said: &str) -> bool {
    let s = said.to_lowercase().replace('’', "'");
    s.contains("out of extra usage")
        || s.contains("out of usage credits")
        || s.contains("extra usage limit")
        || s.contains("monthly spend limit")
        || s.contains("spend limit")
}

impl LimitHit {
    /// What this limit stops.
    pub fn reach(&self) -> Reach {
        reach_of(self.window.as_deref())
    }

    /// The limit a message names: its window when it is not the plan's, and
    /// no reset (words give none that is read).
    pub fn in_words(said: &str) -> LimitHit {
        LimitHit {
            window: window_in_words(said).map(str::to_string),
            resets_at: None,
        }
    }

    /// Its window's length in minutes, as the plan readings say it: the
    /// plan's own 5-hour and weekly windows only. A model's own weekly
    /// limit (`seven_day_opus`, `seven_day_sonnet`) isn't the plan's weekly
    /// window, and has none.
    pub fn minutes(&self) -> Option<u32> {
        match self.window.as_deref()? {
            "five_hour" => Some(300),
            "seven_day" => Some(10_080),
            _ => None,
        }
    }
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
        "rate_limit" if said.is_some_and(|s| says_plan_limit(s) || says_extra_usage(s)) => {
            TurnFailure::Limit(LimitHit::in_words(said.unwrap_or_default()))
        }
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
        // Older wordings: "5-hour limit reached ∙ resets 3pm".
        || s.contains("limit reached ∙ resets")
}

/// How a turn ended as one transcript line says, when the line is Claude
/// Code's API error: the plan's own limit (`apiError: "usage_limit_reached"`,
/// or a `quotaLimits` refusal), with its window and reset; else a failed
/// turn, whatever its words say (an organization's or a passing 429 reads
/// "rate limit" too). None for any other line.
pub fn in_transcript_line(v: &Value) -> Option<TurnFailure> {
    if v.get("isApiErrorMessage").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let quota = v.get("quotaLimits").filter(|q| q.is_object());
    let info = v.pointer("/apiErrorParams/rate_limit_info");
    let plan = v.get("apiError").and_then(Value::as_str) == Some("usage_limit_reached")
        || quota.is_some_and(|q| q.get("status").and_then(Value::as_str) == Some("rejected"));
    if !plan {
        let error = v.get("error").and_then(Value::as_str).unwrap_or("unknown");
        return Some(TurnFailure::Failed(error.to_string()));
    }
    let field = |k: &str| [info, quota].into_iter().flatten().find_map(|o| o.get(k));
    Some(TurnFailure::Limit(LimitHit {
        window: field("rateLimitType")
            .and_then(Value::as_str)
            .map(str::to_string),
        resets_at: field("resetsAt")
            .and_then(Value::as_i64)
            .and_then(|s| DateTime::from_timestamp(s, 0)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_plans_own_windows_have_a_length() {
        let hit = |w: &str| LimitHit {
            window: Some(w.into()),
            resets_at: None,
        };
        assert_eq!(hit("five_hour").minutes(), Some(300));
        assert_eq!(hit("seven_day").minutes(), Some(10_080));
        assert_eq!(hit("seven_day_opus").minutes(), None);
        assert_eq!(hit("seven_day_sonnet").minutes(), None);
    }

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
    fn only_the_plans_own_windows_stop_the_plan() {
        assert_eq!(reach_of(None), Reach::Plan);
        assert_eq!(reach_of(Some("five_hour")), Reach::Plan);
        assert_eq!(reach_of(Some("seven_day")), Reach::Plan);
        assert_eq!(
            reach_of(Some("seven_day_opus")),
            Reach::Model("opus".into())
        );
        assert_eq!(
            reach_of(Some("seven_day_sonnet")),
            Reach::Model("sonnet".into())
        );
        for credit in ["overage", "extra_usage", "monthly", "spend_limit"] {
            assert_eq!(reach_of(Some(credit)), Reach::Credit, "{credit}");
        }
    }

    #[test]
    fn a_limit_message_names_the_window_when_it_is_not_the_plans() {
        let words = |said: &str| of_stop_failure("rate_limit", Some(said));
        let window = |f: TurnFailure| f.limit().and_then(|l| l.window.clone());
        assert_eq!(
            window(words(
                "You're out of extra usage · resets Dec 1, 12am (UTC)"
            )),
            Some("overage".into())
        );
        assert_eq!(
            window(words("You've hit your Opus limit · resets Nov 16, 9am")),
            Some("seven_day_opus".into())
        );
        assert_eq!(
            window(words("You've hit your session limit · resets 9am")),
            None
        );
        assert_eq!(window(words("You've hit your weekly limit")), None);
    }

    #[test]
    fn a_transcript_stop_on_extra_usage_is_not_the_plans() {
        // As Claude Code writes a refusal once the plan's windows are
        // spent and the month's extra usage with them: reset on 1 Dec.
        let line: Value = serde_json::from_str(
            r#"{"type":"assistant","isApiErrorMessage":true,"apiError":"usage_limit_reached",
            "apiErrorParams":{"rate_limit_info":{"status":"rejected","resetsAt":1795996800,
            "rateLimitType":"overage","overageStatus":"rejected","isUsingOverage":true}},
            "quotaLimits":{"status":"rejected","resetsAt":1795996800,"rateLimitType":"overage"},
            "error":"rate_limit"}"#,
        )
        .unwrap();
        let hit = in_transcript_line(&line).unwrap().limit().cloned().unwrap();
        assert_eq!(hit.resets_at, DateTime::from_timestamp(1_795_996_800, 0));
        assert_eq!(hit.reach(), Reach::Credit);
        let opus: Value = serde_json::from_str(
            r#"{"type":"assistant","isApiErrorMessage":true,"apiError":"usage_limit_reached",
            "quotaLimits":{"status":"rejected","resetsAt":1794000000,"rateLimitType":"seven_day_opus"}}"#,
        )
        .unwrap();
        let hit = in_transcript_line(&opus).unwrap().limit().cloned().unwrap();
        assert_eq!(hit.reach(), Reach::Model("opus".into()));
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
            Some(TurnFailure::Limit(LimitHit {
                window: Some("five_hour".into()),
                resets_at: DateTime::from_timestamp(1_792_137_600, 0),
            }))
        );
        // An API key's 429, as 2.1.319 wrote it: a failed turn, never the
        // plan's limit, whatever its words.
        let key: Value = serde_json::from_str(
            r#"{"type":"assistant","isApiErrorMessage":true,"apiError":null,
            "apiErrorParams":null,"quotaLimits":null,"error":"rate_limit","apiErrorStatus":429,
            "message":{"content":[{"type":"text","text":"API Error: Request rejected (429) · This request would exceed your account's rate limit."}]}}"#,
        )
        .unwrap();
        assert_eq!(
            in_transcript_line(&key),
            Some(TurnFailure::Failed("rate_limit".into()))
        );
        // A reply, not an error.
        let reply: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"You've hit your usage limit"}]}}"#,
        )
        .unwrap();
        assert_eq!(in_transcript_line(&reply), None);
    }
}
