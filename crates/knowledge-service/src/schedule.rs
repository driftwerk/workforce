//! Auto-pull / re-index schedule for project mirrors.
//!
//! Two layers: [`ScheduleSpec`] is the raw TOML-friendly shape (every field
//! optional so project + repo blocks compose), and [`EffectiveSchedule`] is
//! the parsed, validated form a scheduler can actually act on.
//!
//! Merging is field-by-field: repo wins when set, project fills in the gaps.
//! Repos opt out with `enabled = false`. A repo with no `path` and no
//! `source` is a configuration error caught at load time elsewhere.
//!
//! Time handling uses the system's local timezone — explicit timezone
//! support is out of scope for the MVP.

use std::time::Duration;

use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use workforce_common::temporal::parse_duration;

/// Raw TOML form. Every field optional so project-level and per-repo blocks
/// merge cleanly.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSpec {
    /// Polling interval string, e.g. `"15m"`, `"1h"`, `"30s"`, `"1d"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<String>,
    /// Active hours window in system local 24h time, e.g. `"9-17"` (inclusive).
    /// When omitted: always active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<String>,
    /// Active weekdays. Strings: `mon|tue|wed|thu|fri|sat|sun` (case-insensitive).
    /// When omitted: every day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekdays: Option<Vec<String>>,
    /// `"incremental"` | `"full"` | `"off"`. Default `incremental`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reindex: Option<String>,
    /// `false` opts the repo out entirely. Inherited from project; per-repo
    /// override stops the scheduler from touching that repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Parsed, validated, ready-to-use schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveSchedule {
    pub every: Duration,
    /// `(start_hour, end_hour)` inclusive both ends, in system local time.
    pub hours: Option<(u32, u32)>,
    /// Bit `i` (0=Mon..6=Sun) set when that day is active.
    pub weekdays: u8,
    pub reindex: ReindexMode,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReindexMode {
    Incremental,
    Full,
    Off,
}

const ALL_WEEKDAYS: u8 = 0b0111_1111;

impl ScheduleSpec {
    /// Field-by-field merge: `self` wins when set, `base` fills the gaps.
    pub fn merge(&self, base: &ScheduleSpec) -> ScheduleSpec {
        ScheduleSpec {
            every: self.every.clone().or_else(|| base.every.clone()),
            hours: self.hours.clone().or_else(|| base.hours.clone()),
            weekdays: self.weekdays.clone().or_else(|| base.weekdays.clone()),
            reindex: self.reindex.clone().or_else(|| base.reindex.clone()),
            enabled: self.enabled.or(base.enabled),
        }
    }

    /// Validate and lower the merged spec into an [`EffectiveSchedule`].
    /// Returns `Ok(None)` when `enabled = false` (repo opted out cleanly).
    pub fn into_effective(self) -> Result<Option<EffectiveSchedule>> {
        if self.enabled == Some(false) {
            return Ok(None);
        }
        let every_str = self
            .every
            .as_deref()
            .ok_or_else(|| eyre!("schedule.every is required (e.g. \"15m\")"))?;
        let every = parse_duration(every_str).map_err(|e| eyre!("schedule.every {e}"))?;
        let hours = self.hours.as_deref().map(parse_hours).transpose()?;
        let weekdays = match self.weekdays.as_ref() {
            Some(list) => parse_weekdays(list)?,
            None => ALL_WEEKDAYS,
        };
        let reindex = match self.reindex.as_deref().unwrap_or("incremental") {
            "incremental" => ReindexMode::Incremental,
            "full" => ReindexMode::Full,
            "off" => ReindexMode::Off,
            other => {
                return Err(eyre!(
                "schedule.reindex must be \"incremental\", \"full\", or \"off\" (got \"{other}\")"
            ))
            }
        };
        Ok(Some(EffectiveSchedule {
            every,
            hours,
            weekdays,
            reindex,
            enabled: true,
        }))
    }
}

impl EffectiveSchedule {
    /// True iff `(weekday_idx, hour)` falls inside the schedule's active
    /// window. `weekday_idx` is 0=Mon..6=Sun.
    pub fn is_active_at(&self, weekday_idx: u32, hour: u32) -> bool {
        if !self.enabled {
            return false;
        }
        if (self.weekdays >> weekday_idx) & 1 == 0 {
            return false;
        }
        match self.hours {
            None => true,
            Some((start, end)) => hour >= start && hour <= end,
        }
    }
}

/// Parse `"9-17"` (inclusive both ends) or a single hour `"9"` into a range.
fn parse_hours(s: &str) -> Result<(u32, u32)> {
    let s = s.trim();
    let (start, end) = match s.split_once('-') {
        Some((a, b)) => (a.trim(), b.trim()),
        None => (s, s),
    };
    let start: u32 = start
        .parse()
        .map_err(|_| eyre!("schedule.hours: bad start \"{start}\""))?;
    let end: u32 = end
        .parse()
        .map_err(|_| eyre!("schedule.hours: bad end \"{end}\""))?;
    if start > 23 || end > 23 {
        return Err(eyre!(
            "schedule.hours: values must be 0..=23 (got {start}-{end})"
        ));
    }
    if start > end {
        return Err(eyre!(
            "schedule.hours: start must not exceed end (got {start}-{end}); \
             overnight windows are not yet supported"
        ));
    }
    Ok((start, end))
}

/// Parse `["mon","tue",...]` into a 7-bit mask. 0=Mon..6=Sun.
fn parse_weekdays(list: &[String]) -> Result<u8> {
    if list.is_empty() {
        return Err(eyre!("schedule.weekdays cannot be an empty list"));
    }
    let mut mask = 0u8;
    for raw in list {
        let bit = match raw.to_ascii_lowercase().as_str() {
            "mon" | "monday" => 0,
            "tue" | "tues" | "tuesday" => 1,
            "wed" | "weds" | "wednesday" => 2,
            "thu" | "thur" | "thurs" | "thursday" => 3,
            "fri" | "friday" => 4,
            "sat" | "saturday" => 5,
            "sun" | "sunday" => 6,
            other => return Err(eyre!("schedule.weekdays: unknown day \"{other}\"")),
        };
        mask |= 1 << bit;
    }
    Ok(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hours_range_and_single() {
        assert_eq!(parse_hours("9-17").unwrap(), (9, 17));
        assert_eq!(parse_hours("0-23").unwrap(), (0, 23));
        assert_eq!(parse_hours("12").unwrap(), (12, 12));
    }

    #[test]
    fn rejects_invalid_hours() {
        assert!(parse_hours("17-9").is_err()); // overnight not supported
        assert!(parse_hours("9-24").is_err()); // out of range
        assert!(parse_hours("a-b").is_err());
    }

    #[test]
    fn parses_weekdays_case_insensitive_and_aliases() {
        let mask = parse_weekdays(&[
            "MON".into(),
            "tues".into(),
            "Wednesday".into(),
            "fri".into(),
        ])
        .unwrap();
        assert_eq!(mask, 0b0001_0111);
    }

    #[test]
    fn rejects_empty_or_unknown_weekdays() {
        assert!(parse_weekdays(&[]).is_err());
        assert!(parse_weekdays(&["funday".into()]).is_err());
    }

    #[test]
    fn merge_repo_wins_when_set_otherwise_inherits() {
        let project = ScheduleSpec {
            every: Some("15m".into()),
            hours: Some("9-17".into()),
            reindex: Some("incremental".into()),
            ..ScheduleSpec::default()
        };
        let repo = ScheduleSpec {
            every: Some("2m".into()),
            ..ScheduleSpec::default()
        };
        let merged = repo.merge(&project);
        assert_eq!(merged.every.as_deref(), Some("2m"));
        assert_eq!(merged.hours.as_deref(), Some("9-17"));
        assert_eq!(merged.reindex.as_deref(), Some("incremental"));
    }

    #[test]
    fn merge_propagates_disable_from_repo_only() {
        let project = ScheduleSpec {
            every: Some("15m".into()),
            ..ScheduleSpec::default()
        };
        let repo = ScheduleSpec {
            enabled: Some(false),
            ..ScheduleSpec::default()
        };
        assert_eq!(repo.merge(&project).enabled, Some(false));
    }

    #[test]
    fn into_effective_when_disabled_returns_none() {
        let spec = ScheduleSpec {
            every: Some("15m".into()),
            enabled: Some(false),
            ..ScheduleSpec::default()
        };
        assert!(spec.into_effective().unwrap().is_none());
    }

    #[test]
    fn into_effective_requires_every() {
        let spec = ScheduleSpec::default();
        let err = spec.into_effective().unwrap_err();
        assert!(err.to_string().contains("schedule.every is required"));
    }

    #[test]
    fn into_effective_defaults() {
        let spec = ScheduleSpec {
            every: Some("15m".into()),
            ..ScheduleSpec::default()
        };
        let eff = spec.into_effective().unwrap().expect("enabled");
        assert_eq!(eff.every, Duration::from_secs(900));
        assert_eq!(eff.hours, None);
        assert_eq!(eff.weekdays, ALL_WEEKDAYS);
        assert_eq!(eff.reindex, ReindexMode::Incremental);
        assert!(eff.enabled);
    }

    #[test]
    fn is_active_at_respects_window() {
        let eff = ScheduleSpec {
            every: Some("15m".into()),
            hours: Some("9-17".into()),
            weekdays: Some(vec!["mon".into(), "tue".into(), "fri".into()]),
            ..ScheduleSpec::default()
        }
        .into_effective()
        .unwrap()
        .unwrap();

        assert!(eff.is_active_at(0, 9)); // Mon 9am
        assert!(eff.is_active_at(0, 17)); // Mon 5pm (inclusive)
        assert!(!eff.is_active_at(0, 8)); // Mon 8am — too early
        assert!(!eff.is_active_at(0, 18)); // Mon 6pm — too late
        assert!(eff.is_active_at(4, 12)); // Fri noon
        assert!(!eff.is_active_at(2, 12)); // Wed — not in mask
        assert!(!eff.is_active_at(5, 12)); // Sat — not in mask
    }
}
