//! Usage-limit reports: read from structured provider data where it exists,
//! else from the notice a CLI prints.

use std::sync::LazyLock;

use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use regex::Regex;
use serde_json::Value;
use workforce_agent::llm::responses::{LimitSnapshot, LimitWindow};
use workforce_common::subscription::SubscriptionKind;

use super::cooldown::{LimitSignal, LimitStatus};

/// Utilisation from which a served call is reported as close to its limit.
const WARN_UTILIZATION: f64 = 0.9;

/// The windows a snapshot reports, by the name Codex gives them.
fn codex_windows(snapshot: &LimitSnapshot) -> impl Iterator<Item = (&'static str, &LimitWindow)> {
    [
        ("primary", &snapshot.primary),
        ("secondary", &snapshot.secondary),
    ]
    .into_iter()
    .filter_map(|(name, window)| window.as_ref().map(|w| (name, w)))
}

/// The window at 100% that frees up last, if one is exhausted.
fn exhausted(snapshot: &LimitSnapshot) -> Option<(&'static str, &LimitWindow)> {
    codex_windows(snapshot)
        .filter(|(_, w)| w.used_percent >= 100.0)
        .max_by_key(|(_, w)| w.resets_at)
}

/// The plan state a ChatGPT response reports: a window at 100% is the limit,
/// one close to it a warning.
pub fn chatgpt_limit(snapshot: &LimitSnapshot) -> Option<LimitSignal> {
    if let Some((name, window)) = exhausted(snapshot) {
        return Some(LimitSignal {
            status: LimitStatus::Rejected,
            resets_at: window.resets_at,
            window: Some(name.to_string()),
        });
    }
    codex_windows(snapshot)
        .filter(|(_, w)| w.used_percent / 100.0 >= WARN_UTILIZATION)
        .max_by(|(_, a), (_, b)| a.used_percent.total_cmp(&b.used_percent))
        .map(|(name, w)| LimitSignal {
            status: LimitStatus::Warning {
                utilization: Some(w.used_percent / 100.0),
            },
            resets_at: w.resets_at,
            window: Some(name.to_string()),
        })
}

/// A ChatGPT `usage_limit_reached` refusal: cooling until the reset its body
/// names, else until the exhausted window's in the last snapshot.
pub fn chatgpt_usage_limit(
    resets_at: Option<DateTime<Utc>>,
    snapshot: Option<&LimitSnapshot>,
) -> LimitSignal {
    let window = snapshot.and_then(exhausted);
    LimitSignal {
        status: LimitStatus::Rejected,
        resets_at: resets_at.or_else(|| window.and_then(|(_, w)| w.resets_at)),
        window: window.map(|(name, _)| name.to_string()),
    }
}

/// The limit state a Claude Code `rate_limit_event` reports: `rejected` is
/// the limit, `allowed_warning` a warning with the window's utilisation,
/// `allowed` nothing.
pub fn claude_rate_limit_event(event: &Value) -> Option<LimitSignal> {
    if event.get("type").and_then(Value::as_str) != Some("rate_limit_event") {
        return None;
    }
    let info = event.get("rate_limit_info")?;
    let window = info
        .get("rateLimitType")
        .and_then(Value::as_str)
        .map(str::to_string);
    let status = match info.get("status").and_then(Value::as_str)? {
        "rejected" => LimitStatus::Rejected,
        "allowed_warning" => LimitStatus::Warning {
            utilization: window
                .as_ref()
                .and_then(|w| info.get("unifiedWindows")?.get(w)?.get("utilization"))
                .and_then(Value::as_f64),
        },
        _ => return None,
    };
    Some(LimitSignal {
        status,
        resets_at: info
            .get("resetsAt")
            .and_then(Value::as_i64)
            .and_then(|at| DateTime::from_timestamp(at, 0)),
        window,
    })
}

/// Where Claude Code's limit notice names its reset: `resets 7pm
/// (Europe/London)`, or `resets Sep 18, 9am (Europe/London)` for a weekly
/// window.
static CLAUDE_RESET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\bresets\s+(?:([a-z]{3})[a-z]*\.?\s+(\d{1,2}),?\s+)?(\d{1,2})(?::(\d{2}))?\s*([ap]m)\s*\(([^)\s]+)\)",
    )
    .expect("the reset pattern compiles")
});

/// Where Codex's limit notice names its retry time: `3:45 PM`, or
/// `Oct 7th, 2026 3:45 PM` when it is not today.
static CODEX_RETRY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\btry again at (?:([a-z]{3}) (\d{1,2})(?:st|nd|rd|th), (\d{4}) )?(\d{1,2}):(\d{2}) ([ap]m)\b",
    )
    .expect("the retry pattern compiles")
});

/// A 12-hour clock time.
fn clock(hour: &str, minute: Option<&str>, meridiem: &str) -> Option<NaiveTime> {
    let hour: u32 = hour.parse().ok().filter(|h| (1..=12).contains(h))?;
    let minute: u32 = minute.map_or(Some(0), |m| m.parse().ok())?;
    let afternoon = if meridiem.eq_ignore_ascii_case("pm") {
        12
    } else {
        0
    };
    NaiveTime::from_hms_opt(hour % 12 + afternoon, minute, 0)
}

/// A date as a notice names it: `Oct`, `7`, `2026`.
fn date(month: &str, day: &str, year: i32) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(&format!("{month} {day} {year}"), "%b %d %Y").ok()
}

/// The first of `days` at `time` in `tz` that is after `now`.
fn first_after<T: TimeZone>(
    days: impl IntoIterator<Item = Option<NaiveDate>>,
    time: NaiveTime,
    tz: &T,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    days.into_iter()
        .flatten()
        .filter_map(|day| tz.from_local_datetime(&day.and_time(time)).earliest())
        .map(|at| at.with_timezone(&Utc))
        .find(|at| *at > now)
}

/// The first `time` in `tz` after `now`: today's, else tomorrow's.
fn next_at<T: TimeZone>(time: NaiveTime, tz: &T, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let today = now.with_timezone(tz).date_naive();
    first_after([Some(today), today.succ_opt()], time, tz, now)
}

/// The first `month` `day` at `time` in `tz` after `now`: this year's, else
/// next year's.
fn next_on<T: TimeZone>(
    month: &str,
    day: &str,
    time: NaiveTime,
    tz: &T,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let year = now.with_timezone(tz).year();
    first_after(
        [date(month, day, year), date(month, day, year + 1)],
        time,
        tz,
        now,
    )
}

/// A Claude Code limit notice (the backstop for a round with no
/// `rate_limit_event`): cooling until the reset it names, or with no reset
/// when it names none it can be read from.
pub fn claude_limit_text(text: &str, now: DateTime<Utc>) -> Option<LimitSignal> {
    is_usage_limit_notice(text).then(|| LimitSignal {
        status: LimitStatus::Rejected,
        resets_at: CLAUDE_RESET.captures(text).and_then(|caps| {
            let time = clock(&caps[3], caps.get(4).map(|m| m.as_str()), &caps[5])?;
            let tz = caps[6].parse::<Tz>().ok()?;
            match (caps.get(1), caps.get(2)) {
                (Some(month), Some(day)) => next_on(month.as_str(), day.as_str(), time, &tz, now),
                _ => next_at(time, &tz, now),
            }
        }),
        window: None,
    })
}

/// A Codex CLI limit failure (`codex exec --json` reports only text):
/// cooling until the retry time it names, in UTC as the round runs it.
pub fn codex_limit_text(text: &str, now: DateTime<Utc>) -> Option<LimitSignal> {
    codex_usage_limit_reached(text).then(|| LimitSignal {
        status: LimitStatus::Rejected,
        resets_at: CODEX_RETRY.captures(text).and_then(|caps| {
            let time = clock(&caps[4], Some(&caps[5]), &caps[6])?;
            match (caps.get(1), caps.get(2), caps.get(3)) {
                (Some(month), Some(day), Some(year)) => {
                    date(month.as_str(), day.as_str(), year.as_str().parse().ok()?)
                        .map(|date| date.and_time(time).and_utc())
                }
                _ => next_at(time, &Utc, now),
            }
        }),
        window: None,
    })
}

/// True when the Claude Code result text reads like a 429 from Anthropic. We
/// match conservatively: the explicit `(429)` token if present, else the
/// rate-limit phrasing that always appears alongside it ("rate limit" within an
/// "API Error" body).
pub fn is_rate_limited(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("(429)")
        || (lower.contains("api error") && lower.contains("rate limit"))
        || lower.contains("throttlingexception")
        || lower.contains("too many requests")
}

/// How Claude Code opens a notice that a subscription window, a spend cap or
/// usage credits stopped the round, lower-cased: the CLI's own list.
const USAGE_LIMIT_OPENINGS: &[&str] = &[
    "you've hit your",
    "you've reached your",
    "you're out of usage credits",
    "you're out of extra usage",
    "your org is out of usage",
    "your seat type doesn't include",
    "your usage allocation has been disabled",
    "your group's usage limit is set to $0",
    "fable 5 requires usage credits",
    "this service is disabled for your org",
];

/// Whether the CLI refused because a usage window, spend cap or credit
/// allowance is spent. Unlike a 429 it does not clear in minutes, so a retry
/// on the same model is wasted. Beyond [`is_usage_limit_notice`], any text
/// saying a limit was reached counts.
pub fn is_usage_capped(text: &str) -> bool {
    let normalised = normalise_notice(text);
    is_usage_limit_notice(text)
        || (normalised.contains("reached your") && normalised.contains("limit"))
}

/// Whether `text` carries a usage-limit notice where the CLI puts one, at the
/// start of a line, so narration that mentions a limit is not one.
pub fn is_usage_limit_notice(text: &str) -> bool {
    normalise_notice(text).lines().map(notice_of).any(|line| {
        USAGE_LIMIT_OPENINGS
            .iter()
            .any(|opening| line.starts_with(opening))
    })
}

fn normalise_notice(text: &str) -> String {
    text.replace(['\u{2018}', '\u{2019}'], "'").to_lowercase()
}

/// A line as the CLI's notice starts it: trimmed, past any blockquote marker
/// a caller quoted it behind, and past the generic "API Error:" prefix the
/// CLI puts in front of messages it relays.
///
/// The blockquote matters because a refusal usually reaches a reader through
/// a halt message, and the narration there is quoted line by line. Without
/// this, every opening below is anchored past a `> ` that was never in the
/// CLI's own output, and only the phrasings with a loose substring fallback
/// are recognised. The ellipsis matters for the same reason: a quoted tail
/// that was truncated opens with one, and a notice landing on that boundary
/// would otherwise be anchored past it.
fn notice_of(line: &str) -> &str {
    let line = line
        .trim()
        .trim_start_matches('…')
        .trim_start_matches("> ")
        .trim_start_matches('…')
        .trim_start();
    line.strip_prefix("api error:")
        .map_or(line, str::trim_start)
}

/// An HTTP auth refusal named by its status: `401` or `403` standing alone,
/// never inside a timestamp, line number or longer number.
static AUTH_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(^|[^0-9a-z.:])40[13]([^0-9a-z.:]|$)").expect("the status pattern compiles")
});

/// Heuristic for an auth-rejection message (401 / unauthorized / invalid token).
pub fn looks_like_auth_failure(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    AUTH_STATUS.is_match(&lower)
        || lower.contains("unauthor")
        || lower.contains("invalid api key")
        || lower.contains("invalid token")
        || lower.contains("oauth token has expired")
        || lower.contains("authentication_error")
        || lower.contains("expiredtoken")
        || lower.contains("unrecognizedclient")
        || lower.contains("accessdeniedexception")
}

/// Codex's own words when the login's usage window is spent
/// (`UsageLimitReached`), lower-cased: the limit that cools the login.
const CODEX_USAGE_LIMIT_MARKERS: &[&str] = &["hit your usage limit", "usage_limit_reached"];

/// Codex's own words when the key's quota is spent (`QuotaExceeded`) or its
/// retries on a rate limit ran out, lower-cased: a throttle, which cools the
/// login briefly rather than for a usage window.
const CODEX_TRANSIENT_LIMIT_MARKERS: &[&str] = &["quota exceeded", "rate limit exceeded"];

/// Whether Codex's failure text says a limit is spent.
pub fn codex_limit_reached(text: &str) -> bool {
    codex_usage_limit_reached(text) || mentions_any(text, CODEX_TRANSIENT_LIMIT_MARKERS)
}

/// Whether Codex's failure text says the login's usage window is spent.
pub fn codex_usage_limit_reached(text: &str) -> bool {
    mentions_any(text, CODEX_USAGE_LIMIT_MARKERS)
}

fn mentions_any(text: &str, markers: &[&str]) -> bool {
    let lower = text.to_lowercase();
    markers.iter().any(|marker| lower.contains(marker))
}

/// What a CLI's failure notice says about the login it ran on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// The plan's usage window, spend cap or credits are spent.
    Capped,
    /// Refused for now: a rate limit that clears in minutes.
    Throttled,
    /// The login was refused.
    Auth,
    Other,
}

/// Read what the CLI a `kind` login runs printed of a failed round —
/// Claude Code for a Claude login, Codex for a ChatGPT one: a limit from its
/// error or its stderr, a refused login from its own error alone, which
/// nothing but the CLI writes.
pub fn classify(kind: SubscriptionKind, error: &str, stderr: &str) -> Notice {
    let limited = |is: fn(&str) -> bool| is(error) || is(stderr);
    let (capped, throttled) = match kind {
        SubscriptionKind::Claude => (limited(is_usage_capped), limited(is_rate_limited)),
        SubscriptionKind::ChatGpt => (
            limited(codex_usage_limit_reached),
            limited(codex_limit_reached),
        ),
    };
    if capped {
        Notice::Capped
    } else if throttled {
        Notice::Throttled
    } else if looks_like_auth_failure(error) {
        Notice::Auth
    } else {
        Notice::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    fn epoch(seconds: i64) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(seconds, 0)
    }

    fn snapshot(pairs: &[(&str, &str)]) -> LimitSnapshot {
        let headers: HeaderMap = pairs
            .iter()
            .map(|(k, v)| {
                (
                    HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                )
            })
            .collect();
        LimitSnapshot::from_headers(&headers).unwrap()
    }

    #[test]
    fn chatgpt_usage_limit_and_codex_headers_parse_reset() {
        let exhausted_primary = snapshot(&[
            ("x-codex-primary-used-percent", "100"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-primary-reset-at", "1790900000"),
        ]);
        let signal = chatgpt_usage_limit(epoch(1_790_956_800), Some(&exhausted_primary));
        assert_eq!(signal.status, LimitStatus::Rejected);
        assert_eq!(signal.resets_at, epoch(1_790_956_800));
        assert_eq!(signal.window.as_deref(), Some("primary"));
        assert_eq!(
            chatgpt_usage_limit(None, Some(&exhausted_primary)).resets_at,
            epoch(1_790_900_000),
            "no reset in the body: the exhausted window's"
        );

        let from_headers = chatgpt_limit(&snapshot(&[
            ("x-codex-primary-used-percent", "42.5"),
            ("x-codex-secondary-used-percent", "100.0"),
            ("x-codex-secondary-reset-at", "1791000000"),
        ]))
        .unwrap();
        assert_eq!(from_headers.status, LimitStatus::Rejected);
        assert_eq!(from_headers.resets_at, epoch(1_791_000_000));
        assert_eq!(from_headers.window.as_deref(), Some("secondary"));
    }

    #[test]
    fn chatgpt_headers_below_the_limit_warn_or_stay_quiet() {
        let warning = chatgpt_limit(&snapshot(&[
            ("x-codex-primary-used-percent", "93"),
            ("x-codex-primary-reset-at", "1791000000"),
        ]))
        .unwrap();
        assert_eq!(
            warning.status,
            LimitStatus::Warning {
                utilization: Some(0.93)
            }
        );
        assert_eq!(
            chatgpt_limit(&snapshot(&[("x-codex-primary-used-percent", "12")])),
            None
        );
    }

    /// A `rate_limit_event` as Claude Code 2.1.284 streamed it on a live
    /// round (captured 2026-09-29): every field here is the CLI's own.
    const CAPTURED_ALLOWED: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1790676600,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false,"unifiedWindows":{"five_hour":{"utilization":0.06,"resetsAt":1790676600},"seven_day":{"utilization":0.69,"resetsAt":1790956800}}},"uuid":"9d3035c2-ea78-49c3-980d-cf126bcd660f","session_id":"a4e6e266-c630-4af1-a014-b75cfe91ea9f"}"#;

    /// The captured event with its `status` (and window) replaced: no round
    /// has been captured at or near its limit, so `rejected` and
    /// `allowed_warning` come from the documented schema, on the captured
    /// shape.
    fn event(status: &str, window: &str) -> Value {
        let mut event: Value = serde_json::from_str(CAPTURED_ALLOWED).unwrap();
        event["rate_limit_info"]["status"] = status.into();
        event["rate_limit_info"]["rateLimitType"] = window.into();
        event["rate_limit_info"]["resetsAt"] = 1_790_956_800.into();
        event
    }

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339).unwrap().to_utc()
    }

    #[test]
    fn claude_rate_limit_event_rejected_sets_cooldown_until_resets_at() {
        let captured: Value = serde_json::from_str(CAPTURED_ALLOWED).unwrap();
        assert_eq!(claude_rate_limit_event(&captured), None, "allowed is quiet");

        let rejected = claude_rate_limit_event(&event("rejected", "seven_day")).unwrap();
        assert_eq!(
            rejected,
            LimitSignal {
                status: LimitStatus::Rejected,
                resets_at: epoch(1_790_956_800),
                window: Some("seven_day".into()),
            }
        );
        let store = super::super::cooldown::CooldownStore::default();
        let key = super::super::resolve::LoginKey::of(&super::super::resolve::Credential::Token(
            "sk-ant-oat01-limit-event".into(),
        ))
        .unwrap();
        let now = at("2026-09-29T12:00:00Z");
        store.note(&key, &rejected, now);
        assert_eq!(store.until(&key, now), epoch(1_790_956_800));

        assert_eq!(
            claude_rate_limit_event(&event("allowed_warning", "seven_day"))
                .unwrap()
                .status,
            LimitStatus::Warning {
                utilization: Some(0.69)
            },
            "a warning carries its window's utilisation"
        );
        assert_eq!(
            claude_rate_limit_event(&serde_json::json!({"type": "result"})),
            None
        );
    }

    #[test]
    fn claude_limit_text_parses_reset_in_named_zone() {
        let now = at("2026-09-29T12:00:00Z");
        let signal = claude_limit_text(
            "You've hit your session limit · resets 7pm (Europe/London)",
            now,
        )
        .unwrap();
        assert_eq!(signal.status, LimitStatus::Rejected);
        assert_eq!(
            signal.resets_at,
            Some(at("2026-09-29T18:00:00Z")),
            "19:00 BST is 18:00 UTC"
        );
        assert_eq!(
            claude_limit_text(
                "You've hit your session limit · resets 7:30am (America/New_York)",
                now
            )
            .unwrap()
            .resets_at,
            Some(at("2026-09-30T11:30:00Z")),
            "07:30 EDT has passed today, so it is tomorrow's"
        );
        assert_eq!(
            claude_limit_text("You've hit your weekly limit", now)
                .unwrap()
                .resets_at,
            None,
            "no reset named: the default window applies"
        );
        assert_eq!(
            claude_limit_text(
                "You've hit your weekly limit · resets Oct 2, 9am (Europe/London)",
                now
            )
            .unwrap()
            .resets_at,
            Some(at("2026-10-02T08:00:00Z")),
            "a weekly notice names its day"
        );
        assert_eq!(
            claude_limit_text(
                "You've hit your weekly limit · resets Sep 18, 9am (UTC)",
                now
            )
            .unwrap()
            .resets_at,
            Some(at("2027-09-18T09:00:00Z")),
            "a day already past this year is next year's"
        );
        assert_eq!(
            claude_limit_text("This service is disabled for your org", now).map(|s| s.status),
            Some(LimitStatus::Rejected)
        );
        for narration in [
            "I checked the rate limit docs; resets 7pm (Europe/London)",
            "The banner says you have reached your plan limit",
        ] {
            assert_eq!(
                claude_limit_text(narration, now),
                None,
                "narration that mentions a limit is not a limit: {narration}"
            );
        }
    }

    #[test]
    fn codex_limit_text_parses_utc_today_tomorrow_and_dated() {
        let now = at("2026-09-29T12:00:00Z");
        let today = "You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at 3:45 PM.";
        assert_eq!(
            codex_limit_text(today, now).unwrap().resets_at,
            Some(at("2026-09-29T15:45:00Z"))
        );
        let tomorrow = "You've hit your usage limit. Try again at 9:05 AM.";
        assert_eq!(
            codex_limit_text(tomorrow, now).unwrap().resets_at,
            Some(at("2026-09-30T09:05:00Z"))
        );
        let dated = "You've hit your usage limit. Try again at Oct 3rd, 2026 12:15 AM.";
        assert_eq!(
            codex_limit_text(dated, now).unwrap().resets_at,
            Some(at("2026-10-03T00:15:00Z"))
        );
        assert_eq!(
            codex_limit_text("You've hit your usage limit. Try again later.", now)
                .unwrap()
                .resets_at,
            None
        );
        assert_eq!(
            codex_limit_text("the model refused: try again at 3:45 PM", now),
            None
        );
    }

    #[test]
    fn classify_reads_each_clis_notice() {
        use SubscriptionKind::{ChatGpt, Claude};
        for (kind, notice, read) in [
            (
                Claude,
                "You've hit your weekly limit · resets Sep 18, 9am",
                Notice::Capped,
            ),
            (
                Claude,
                "API Error: Request rejected (429)",
                Notice::Throttled,
            ),
            (Claude, "API Error: 401 authentication_error", Notice::Auth),
            (Claude, "segfault", Notice::Other),
            (
                ChatGpt,
                "You've hit your usage limit. Try again later.",
                Notice::Capped,
            ),
            (
                ChatGpt,
                "Quota exceeded. Check your plan and billing details.",
                Notice::Throttled,
            ),
            (ChatGpt, "unexpected status 401 Unauthorized", Notice::Auth),
            (ChatGpt, "the spec failed", Notice::Other),
        ] {
            assert_eq!(classify(kind, notice, ""), read, "{kind:?}: {notice}");
        }
    }

    #[test]
    fn auth_reads_the_clis_own_error_and_whole_status_codes() {
        use SubscriptionKind::Claude;
        let stderr = "2026-10-02T10:14:12.401Z WARN mcp: 401 Unauthorized from a tool server";
        assert_eq!(classify(Claude, "", stderr), Notice::Other);
        for not_a_status in ["took 1401ms", "line 401:7", "at 12:00:00.401", "v4.0.1"] {
            assert!(!looks_like_auth_failure(not_a_status), "{not_a_status}");
        }
        for status in ["API Error: 401 Unauthorized", "status 403", "(401)"] {
            assert!(looks_like_auth_failure(status), "{status}");
        }
    }

    #[test]
    fn codex_transient_limits_name_no_usage_window() {
        let now = at("2026-09-29T12:00:00Z");
        for transient in [
            "rate limit exceeded: retry later",
            "Quota exceeded. Check your plan and billing details.",
        ] {
            assert!(codex_limit_reached(transient));
            assert_eq!(codex_limit_text(transient, now), None, "{transient}");
        }
    }

    #[test]
    fn is_rate_limited_matches_anthropic_429_message() {
        // The actual phrasing Anthropic surfaces in Claude Code's result text
        // when the org's input TPM cap is exceeded.
        let real = "API Error: Request rejected (429) · This request would exceed your organization's rate limit of 30,000 input tokens per minute (org: 1a56c77e-..., model: claude-sonnet-4-6).";
        assert!(is_rate_limited(real));
    }

    #[test]
    fn is_rate_limited_matches_other_rate_limit_phrasings() {
        // Anthropic doesn't always include the explicit `(429)` token; the
        // shape we keep stable across versions is "API Error" + "rate limit".
        assert!(is_rate_limited(
            "API Error: rate limit exceeded for input tokens"
        ));
    }

    #[test]
    fn is_usage_capped_matches_every_notice_the_cli_opens_a_limit_with() {
        for notice in [
            "You've hit your weekly limit · resets Sep 18, 9am",
            "You\u{2019}ve hit your weekly limit · resets Sep 18, 9am",
            "You've hit your monthly spend limit · your team limit resets Oct 1",
            "You're out of usage credits. Switch to another model to continue.",
            "Your org is out of usage · contact your admin",
            "Fable 5 requires usage credits",
            "You've reached your Fable limit. Switch to another model to continue.",
            "API Error: 429 overloaded\nYou've hit your session limit · resets 7pm",
            "API Error: You've hit your weekly limit · resets Sep 18, 9am",
            "  api error:   You\u{2019}re out of extra usage",
            "Lower-priority mode ended · you have reached your weekly usage limit",
            r#"API Error: 400 {"type":"error","error":{"type":"invalid_request_error","message":"You have reached your specified API usage limits. You will regain access on 2026-10-01 at 00:00 UTC."}}"#,
        ] {
            assert!(is_usage_capped(notice), "{notice}");
        }
        for other in [
            "API Error: Request rejected (429)",
            "the limit was reached by the loop",
            "I think the loop hit your limit on retries",
            "API Error: spend limit reached (daily; resets 2026-08-08 00:00 UTC)",
            "Error: You've hit your weekly limit",
            "",
        ] {
            assert!(!is_usage_capped(other), "{other}");
        }
    }

    #[test]
    fn is_rate_limited_rejects_unrelated_failures() {
        assert!(!is_rate_limited("API Error: invalid_request_error"));
        assert!(!is_rate_limited("connection refused"));
        assert!(!is_rate_limited(""));
        // "rate limit" alone, without the API Error context, isn't enough —
        // model output discussing rate limits shouldn't trigger our retry.
        assert!(!is_rate_limited(
            "the system has a rate limit but that's expected"
        ));
    }

    #[test]
    fn bedrock_throttling_reads_as_a_rate_limit_and_expired_credentials_as_auth() {
        assert!(is_rate_limited(
            "API Error: ThrottlingException: Too many tokens, please wait before trying again."
        ));
        assert!(is_rate_limited("429 Too Many Requests"));
        assert!(looks_like_auth_failure(
            "ExpiredTokenException: The security token included in the request is expired"
        ));
        assert!(looks_like_auth_failure(
            "AccessDeniedException: not authorized to invoke"
        ));
    }
}
