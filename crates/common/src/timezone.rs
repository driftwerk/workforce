use chrono::{DateTime, Utc};
use chrono_tz::Tz;

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
