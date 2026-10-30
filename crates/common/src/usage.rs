//! Plan usage as the plans' own clients report it: how full each window of
//! a Claude or ChatGPT plan is, and when it resets.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanKind {
    Claude,
    Chatgpt,
}

impl PlanKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Chatgpt => "chatgpt",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Self::Claude),
            "chatgpt" => Some(Self::Chatgpt),
            _ => None,
        }
    }
}

/// One window of a plan: how much of it is used, 0 to 100, and when it
/// starts over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub minutes: u32,
}

/// Claude's five-hour window, in minutes.
pub const FIVE_HOUR: u32 = 300;
/// Claude's weekly window, in minutes.
pub const SEVEN_DAY: u32 = 10_080;

/// How long a Claude window is, by the name Claude Code gives it; Claude
/// sends no lengths.
pub fn claude_window(kind: &str) -> Option<u32> {
    match kind {
        "five_hour" => Some(FIVE_HOUR),
        "seven_day" => Some(SEVEN_DAY),
        _ => None,
    }
}

/// `windows`, shortest first.
pub fn ordered(mut windows: Vec<Window>) -> Vec<Window> {
    windows.sort_by_key(|w| w.minutes);
    windows
}

/// A percentage to a tenth, as the plans' clients show it.
pub fn percent(used: f64) -> f64 {
    (used * 10.0).round() / 10.0
}

/// The windows Desk's plugin read from a Claude Code session:
/// `rateLimits` of `{kind, percentUsed, resetsAt}`, the reset an ISO 8601
/// time. A window of a kind with no known length is left out.
pub fn claude_mod_usage(limits: &Value) -> Vec<Window> {
    let windows = limits
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            Some(Window {
                used_percent: percent(l.get("percentUsed")?.as_f64()?),
                resets_at: l
                    .get("resetsAt")
                    .and_then(Value::as_str)
                    .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                    .map(|at| at.to_utc()),
                minutes: claude_window(l.get("kind")?.as_str()?)?,
            })
        })
        .collect();
    ordered(windows)
}

/// The windows of a Codex rate-limit snapshot: `rateLimits.primary` and
/// `.secondary`, each `{usedPercent, windowDurationMins, resetsAt}`, the
/// reset in epoch seconds. A window without a length is left out.
pub fn codex_usage(snapshot: &Value) -> Vec<Window> {
    let windows = ["primary", "secondary"]
        .into_iter()
        .filter_map(|name| {
            let w = snapshot.pointer(&format!("/rateLimits/{name}"))?;
            Some(Window {
                used_percent: percent(w.get("usedPercent")?.as_f64()?),
                resets_at: w
                    .get("resetsAt")
                    .and_then(Value::as_i64)
                    .and_then(|at| DateTime::from_timestamp(at, 0)),
                minutes: u32::try_from(w.get("windowDurationMins")?.as_i64()?).ok()?,
            })
        })
        .collect();
    ordered(windows)
}

/// The key a plan's account is known by across hubs: a hash of its ids, so
/// the ids themselves never leave the hub. `None` without any.
pub fn account_key(kind: PlanKind, ids: &[String]) -> Option<String> {
    let ids: Vec<&str> = ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(kind.as_str());
    ids.iter().for_each(|id| {
        hash.update([0]);
        hash.update(id);
    });
    Some(hex::encode(&hash.finalize()[..8]))
}

/// Which login a reading is of: one in a hub's vault (keyed, and the person
/// it is per-user for), or this Mac's own sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginRef {
    Hub {
        login: String,
        owner: Option<String>,
    },
    Mac,
}

impl LoginRef {
    /// The person the login is per-user for; none for a shared login or
    /// this Mac's.
    pub fn owner(&self) -> Option<&str> {
        match self {
            Self::Hub { owner, .. } => owner.as_deref(),
            Self::Mac => None,
        }
    }
}

/// How full a plan's windows were when it was last used.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub kind: PlanKind,
    pub login: LoginRef,
    pub account: Option<String>,
    pub plan: Option<String>,
    pub windows: Vec<Window>,
    pub observed_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn at(rfc3339: &str) -> Option<DateTime<Utc>> {
        Some(DateTime::parse_from_rfc3339(rfc3339).unwrap().to_utc())
    }

    #[test]
    fn a_claude_sessions_limits_are_its_windows() {
        let limits = json!([
            {"kind": "seven_day", "percentUsed": 38, "resetsAt": "2026-11-02T09:00:00Z"},
            {"kind": "five_hour", "percentUsed": 62.5, "resetsAt": "2026-10-29T19:10:00Z"},
            {"kind": "spend_limit", "percentUsed": 3}
        ]);
        assert_eq!(
            claude_mod_usage(&limits),
            [
                Window {
                    used_percent: 62.5,
                    resets_at: at("2026-10-29T19:10:00Z"),
                    minutes: FIVE_HOUR
                },
                Window {
                    used_percent: 38.0,
                    resets_at: at("2026-11-02T09:00:00Z"),
                    minutes: SEVEN_DAY
                },
            ]
        );
        assert_eq!(
            claude_mod_usage(&json!([{"kind": "five_hour", "percentUsed": 7}])),
            [Window {
                used_percent: 7.0,
                resets_at: None,
                minutes: FIVE_HOUR
            }],
            "a window without its reset"
        );
        assert!(claude_mod_usage(&json!(null)).is_empty());
    }

    #[test]
    fn a_codex_snapshot_is_its_windows() {
        let snapshot = json!({"rateLimits": {
            "primary": {"usedPercent": 12, "windowDurationMins": 300, "resetsAt": 1793300000},
            "secondary": {"usedPercent": 40, "windowDurationMins": null, "resetsAt": 1793800000}}});
        assert_eq!(
            codex_usage(&snapshot),
            [Window {
                used_percent: 12.0,
                resets_at: DateTime::from_timestamp(1_793_300_000, 0),
                minutes: 300
            }],
            "a window without a length is left out"
        );
    }

    #[test]
    fn an_account_is_keyed_without_its_ids() {
        let ids = vec!["org-1".to_string(), "person@example.com".to_string()];
        let key = account_key(PlanKind::Claude, &ids).unwrap();
        assert_eq!(key.len(), 16);
        assert!(!key.contains("org-1"));
        assert_eq!(account_key(PlanKind::Claude, &ids), Some(key.clone()));
        assert_ne!(account_key(PlanKind::Chatgpt, &ids), Some(key));
        assert_eq!(account_key(PlanKind::Claude, &[" ".into()]), None);
    }
}
