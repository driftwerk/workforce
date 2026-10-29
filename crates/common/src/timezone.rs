use chrono::{DateTime, Datelike, NaiveTime, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// Return the current time in the given user timezone.
pub fn now_in_user_tz(tz: Tz) -> DateTime<Tz> {
    Utc::now().with_timezone(&tz)
}

/// Format a datetime for display in a system prompt.
///
/// Produces a human-readable string like:
///   "Wednesday, 11th February 2026 — 8:35 AM (Europe/London, GMT)"
pub fn format_for_prompt(dt: &DateTime<Tz>) -> String {
    let day_of_week = dt.format("%A");
    let day = dt.format("%-d").to_string();
    let suffix = ordinal_suffix(&day);
    let month_year = dt.format("%B %Y");
    let time = dt.format("%-I:%M %p");
    let tz_name = dt.timezone();
    let tz_abbrev = dt.format("%Z");

    format!(
        "{}, {}{} {} — {} ({}, {})",
        day_of_week, day, suffix, month_year, time, tz_name, tz_abbrev,
    )
}

/// Build the "Current Date & Time" section for a system prompt.
pub fn system_prompt_section(tz: Tz) -> String {
    let now_user = now_in_user_tz(tz);
    let formatted = format_for_prompt(&now_user);
    let user_tz_name = tz.to_string();

    let mut section = String::from("## Current Date & Time\n");
    section.push_str(&format!("Current time: {}\n", formatted));
    section.push_str("Server timezone: UTC\n");
    section.push_str(&format!("User timezone: {}\n", user_tz_name));
    // A long-lived conversation carries every time ever mentioned in it, and a
    // model asked the time will happily repeat one of those instead of reading
    // this line: a session running overnight answered "9:40 PM" at half past
    // six the next morning, which is exactly what it had said the evening
    // before. The line below is cheap and the failure is not obvious, because a
    // confidently stated wrong time looks like a broken clock rather than a
    // misread prompt.
    section.push_str(
        "This block is regenerated for every message and is the only correct source for \
         the current date and time. Any date or time mentioned earlier in this conversation \
         was current when it was written and is now out of date — never repeat one as if it \
         were now, and never infer the time from how long a conversation feels.\n\n\
         Read dated information newest first. Daily notes, memories and earlier messages all \
         carry the date they describe: work back from the most recent, and when two of them \
         disagree the newer one wins — including when the older is longer, more detailed or \
         more confidently written, which it usually is. An older entry says how things were \
         then, never how they are now. Asked what is true today, answer from today's \
         information and say so if today has nothing to offer, rather than reaching back for \
         a fuller answer from a day that has passed.",
    );
    section
}

/// Parse an `HH:MM` wall-clock time ("08:00", "23:30").
pub fn parse_hhmm(s: &str) -> Option<NaiveTime> {
    let (hour, minute) = s.split_once(':')?;
    NaiveTime::from_hms_opt(hour.parse().ok()?, minute.parse().ok()?, 0)
}

/// Whether `t` falls in `[start, end)`; an `end` before `start` wraps midnight.
pub fn time_in_window(t: NaiveTime, start: NaiveTime, end: NaiveTime) -> bool {
    if start <= end {
        start <= t && t < end
    } else {
        t >= start || t < end
    }
}

/// A recurring wall-clock window in a named timezone: `start` to `end` on each
/// of `days` (every day when empty). A window whose `end` is before its
/// `start` wraps midnight and belongs to the day it opens on; one whose `end`
/// equals its `start` is the whole day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ActiveHoursSpec", into = "ActiveHoursSpec")]
pub struct ActiveHours {
    pub days: Vec<Weekday>,
    pub start: NaiveTime,
    pub end: NaiveTime,
    pub tz: Tz,
}

/// [`ActiveHours`] as written: `days = ["mon", …]`, `start`/`end` as `HH:MM`,
/// `tz` an IANA name.
#[derive(Serialize, Deserialize)]
struct ActiveHoursSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    days: Vec<String>,
    start: String,
    end: String,
    tz: String,
}

impl TryFrom<ActiveHoursSpec> for ActiveHours {
    type Error = String;

    fn try_from(spec: ActiveHoursSpec) -> Result<Self, Self::Error> {
        let time = |field: &str, value: &str| {
            parse_hhmm(value).ok_or_else(|| format!("active_hours {field} '{value}' is not HH:MM"))
        };
        let start = time("start", &spec.start)?;
        let end = time("end", &spec.end)?;
        let days = spec
            .days
            .iter()
            .map(|day| {
                day.parse::<Weekday>().map_err(|_| {
                    format!("active_hours day '{day}' is not a weekday (mon, tue, … sun)")
                })
            })
            .collect::<Result<_, _>>()?;
        let tz = spec.tz.parse::<Tz>().map_err(|_| {
            format!(
                "active_hours tz '{}' is not an IANA timezone (e.g. Europe/London)",
                spec.tz
            )
        })?;
        Ok(Self {
            days,
            start,
            end,
            tz,
        })
    }
}

impl From<ActiveHours> for ActiveHoursSpec {
    fn from(hours: ActiveHours) -> Self {
        Self {
            days: hours
                .days
                .iter()
                .map(|day| day.to_string().to_lowercase())
                .collect(),
            start: hours.start.format("%H:%M").to_string(),
            end: hours.end.format("%H:%M").to_string(),
            tz: hours.tz.name().to_string(),
        }
    }
}

impl ActiveHours {
    /// Whether `at` falls inside the window.
    pub fn contains(&self, at: DateTime<Utc>) -> bool {
        let local = at.with_timezone(&self.tz);
        let t = local.time();
        if self.start != self.end && !time_in_window(t, self.start, self.end) {
            return false;
        }
        let opened_on = if self.start > self.end && t < self.end {
            local.weekday().pred()
        } else {
            local.weekday()
        };
        self.days.is_empty() || self.days.contains(&opened_on)
    }
}

/// Return the English ordinal suffix for a numeric day string ("1" → "st", "2" → "nd", etc.).
fn ordinal_suffix(day: &str) -> &'static str {
    let n: u32 = day.parse().unwrap_or(0);
    match (n % 10, n % 100) {
        (1, 11) => "th",
        (2, 12) => "th",
        (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    }
}

#[cfg(test)]
mod tests {
    /// The failure this guards: asked what someone was doing today, the agent
    /// answered from a note headed "Yesterday" — 3,862 characters of it —
    /// while today's note, which held the right answer, was 714 characters and
    /// sat below it. Size and confidence are not recency, and the rule has to
    /// say so because the wrong answer is always the better-written one.
    #[test]
    fn dated_information_is_to_be_read_newest_first() {
        let section = system_prompt_section(chrono_tz::Europe::London);
        assert!(section.contains("newest first"), "{section}");
        assert!(section.contains("the newer one wins"), "{section}");
        assert!(
            section.contains("longer, more detailed"),
            "must name why the older entry wins by default: {section}"
        );
    }

    /// The failure this guards: a session left running overnight answered
    /// "9:40 PM" at half past six the next morning — the time it had given the
    /// evening before, still sitting in the conversation. The prompt carried
    /// the right time throughout.
    #[test]
    fn the_time_block_disowns_times_from_earlier_in_the_conversation() {
        let section = system_prompt_section(chrono_tz::Europe::London);
        assert!(
            section.contains("regenerated for every message"),
            "{section}"
        );
        assert!(
            section.contains("never repeat one as if it were now"),
            "{section}"
        );
    }

    use super::*;
    use chrono::TimeZone;

    fn hours(toml: &str) -> Result<ActiveHours, String> {
        toml::from_str::<ActiveHours>(toml).map_err(|e| e.to_string())
    }

    fn london(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        chrono_tz::Europe::London
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn parse_hhmm_reads_wall_clock_times() {
        assert_eq!(parse_hhmm("08:00"), NaiveTime::from_hms_opt(8, 0, 0));
        assert_eq!(parse_hhmm("23:59"), NaiveTime::from_hms_opt(23, 59, 0));
        for bad in ["25:00", "12:60", "abc", "", "12", "1:2:3"] {
            assert_eq!(parse_hhmm(bad), None, "{bad}");
        }
    }

    #[test]
    fn active_hours_weekdays_in_local_time() {
        let h = hours(
            "days = [\"mon\", \"Tue\", \"wednesday\", \"thu\", \"fri\"]\n\
             start = \"10:00\"\nend = \"16:00\"\ntz = \"Europe/London\"",
        )
        .unwrap();
        // Thursday 29 Oct 2026, GMT.
        assert!(h.contains(london(2026, 10, 29, 10, 0)));
        assert!(h.contains(london(2026, 10, 29, 15, 59)));
        assert!(!h.contains(london(2026, 10, 29, 16, 0)));
        assert!(!h.contains(london(2026, 10, 29, 9, 59)));
        // Saturday.
        assert!(!h.contains(london(2026, 10, 31, 12, 0)));
        // BST: 10:30 local is 09:30 UTC.
        assert!(h.contains(london(2026, 7, 15, 10, 30)));
        assert!(!h.contains(london(2026, 7, 15, 9, 30)));
    }

    #[test]
    fn active_hours_wrapping_belongs_to_the_day_it_opens() {
        let h =
            hours("days = [\"fri\"]\nstart = \"22:00\"\nend = \"06:00\"\ntz = \"UTC\"").unwrap();
        let at = |d, hh| Utc.with_ymd_and_hms(2026, 10, d, hh, 0, 0).unwrap();
        assert!(h.contains(at(30, 23)), "Friday night");
        assert!(h.contains(at(31, 2)), "early Saturday, opened Friday");
        assert!(!h.contains(at(31, 23)), "Saturday night");
        assert!(!h.contains(at(30, 2)), "early Friday, opened Thursday");
    }

    #[test]
    fn active_hours_ending_where_they_start_are_the_whole_day() {
        let h =
            hours("days = [\"sat\"]\nstart = \"00:00\"\nend = \"00:00\"\ntz = \"UTC\"").unwrap();
        let at = |d, hh, mm| Utc.with_ymd_and_hms(2026, 10, d, hh, mm, 0).unwrap();
        assert!(h.contains(at(31, 0, 0)));
        assert!(h.contains(at(31, 23, 59)));
        assert!(!h.contains(at(30, 23, 59)), "Friday");
        assert!(
            !h.contains(Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap()),
            "Sunday"
        );
    }

    #[test]
    fn active_hours_without_days_is_every_day() {
        let h = hours("start = \"10:00\"\nend = \"16:00\"\ntz = \"UTC\"").unwrap();
        assert!(h.days.is_empty());
        assert!(h.contains(Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap()));
    }

    #[test]
    fn active_hours_refuses_what_it_cannot_read() {
        let err = |toml: &str| hours(toml).unwrap_err();
        assert!(err("start = \"9\"\nend = \"16:00\"\ntz = \"UTC\"").contains("start '9'"));
        assert!(
            err("days = [\"funday\"]\nstart = \"10:00\"\nend = \"16:00\"\ntz = \"UTC\"")
                .contains("'funday'")
        );
        assert!(err("start = \"10:00\"\nend = \"16:00\"\ntz = \"London\"").contains("IANA"));
        assert!(err("start = \"10:00\"\nend = \"16:00\"").contains("tz"));
    }

    #[test]
    fn active_hours_round_trips_in_its_written_form() {
        let h = hours(
            "days = [\"Monday\", \"fri\"]\nstart = \"07:05\"\nend = \"16:00\"\ntz = \"Europe/London\"",
        )
        .unwrap();
        let written = toml::to_string(&h).unwrap();
        assert!(written.contains("days = [\"mon\", \"fri\"]"), "{written}");
        assert!(written.contains("start = \"07:05\""), "{written}");
        assert!(written.contains("tz = \"Europe/London\""), "{written}");
        assert_eq!(hours(&written).unwrap(), h);
    }

    #[test]
    fn ordinal_suffixes() {
        assert_eq!(ordinal_suffix("1"), "st");
        assert_eq!(ordinal_suffix("2"), "nd");
        assert_eq!(ordinal_suffix("3"), "rd");
        assert_eq!(ordinal_suffix("4"), "th");
        assert_eq!(ordinal_suffix("11"), "th");
        assert_eq!(ordinal_suffix("12"), "th");
        assert_eq!(ordinal_suffix("13"), "th");
        assert_eq!(ordinal_suffix("21"), "st");
        assert_eq!(ordinal_suffix("22"), "nd");
        assert_eq!(ordinal_suffix("23"), "rd");
        assert_eq!(ordinal_suffix("31"), "st");
    }

    #[test]
    fn now_in_user_tz_returns_correct_timezone() {
        let tz: Tz = "Europe/London".parse().expect("valid tz");
        let dt = now_in_user_tz(tz);
        assert_eq!(dt.timezone(), tz);
    }

    #[test]
    fn format_for_prompt_fixed_date() {
        let tz: Tz = "Europe/London".parse().expect("valid tz");
        // 2026-02-11 08:35 UTC — London is GMT in February (no DST)
        let utc_dt = Utc.with_ymd_and_hms(2026, 2, 11, 8, 35, 0).unwrap();
        let london_dt = utc_dt.with_timezone(&tz);
        let formatted = format_for_prompt(&london_dt);

        assert!(
            formatted.contains("Wednesday"),
            "should contain day of week: {formatted}"
        );
        assert!(
            formatted.contains("11th"),
            "should contain ordinal day: {formatted}"
        );
        assert!(
            formatted.contains("February"),
            "should contain month: {formatted}"
        );
        assert!(
            formatted.contains("2026"),
            "should contain year: {formatted}"
        );
        assert!(
            formatted.contains("8:35 AM"),
            "should contain time: {formatted}"
        );
        assert!(
            formatted.contains("Europe/London"),
            "should contain tz name: {formatted}"
        );
        assert!(
            formatted.contains("GMT"),
            "should contain tz abbrev: {formatted}"
        );
    }

    #[test]
    fn format_for_prompt_dst() {
        let tz: Tz = "America/New_York".parse().expect("valid tz");
        // 2026-07-15 18:00 UTC = 2:00 PM EDT in New York
        let utc_dt = Utc.with_ymd_and_hms(2026, 7, 15, 18, 0, 0).unwrap();
        let ny_dt = utc_dt.with_timezone(&tz);
        let formatted = format_for_prompt(&ny_dt);

        assert!(formatted.contains("Wednesday"), "day of week: {formatted}");
        assert!(formatted.contains("2:00 PM"), "time: {formatted}");
        assert!(
            formatted.contains("America/New_York"),
            "tz name: {formatted}"
        );
        assert!(
            formatted.contains("EDT"),
            "tz abbrev during DST: {formatted}"
        );
    }

    #[test]
    fn system_prompt_section_contains_all_parts() {
        let tz: Tz = "Asia/Tokyo".parse().expect("valid tz");
        let section = system_prompt_section(tz);

        assert!(section.contains("## Current Date & Time"));
        assert!(section.contains("Current time:"));
        assert!(section.contains("Server timezone: UTC"));
        assert!(section.contains("User timezone: Asia/Tokyo"));
    }

    #[test]
    fn utc_fallback() {
        let section = system_prompt_section(chrono_tz::UTC);
        assert!(section.contains("User timezone: UTC"));
    }
}
