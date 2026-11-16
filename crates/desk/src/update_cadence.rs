//! When a boss tells the person what happened (`[oversight.updates]`'s
//! cadence, [`Cadence`]): each thing it would say is one of five kinds of
//! event ([`EventKind`]), and the person's cadence routes each right away
//! (a message of its own, shape A), into the next roundup (one message an
//! interval, shape C), or to be said only when they ask.
//!
//! - **Quiet hours** hold what would come right away for the morning's
//!   roundup, except what needs the person when they said so.
//! - **Decisions** are told by the person's choice: the important ones
//!   ([`Important`]: it merged, released, deleted, spent, overrode a
//!   session, or set something to happen later) in the roundup, right away,
//!   every one, or only on asking. The rest are routine.
//! - **A roundup** comes on the clock (every 30 minutes, hour or two hours,
//!   or at morning, lunch and end of day), never in the quiet hours, and is
//!   skipped when it would say nothing ([`Roundup::is_empty`]).

use chrono::{DateTime, Duration, NaiveTime, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use workforce_common::oversight::{Cadence, Decided, RoundupEvery, Route};

use crate::update_write::{EventKind, Said};

/// Why a decision is important enough to be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Important {
    Merged,
    Released,
    Deleted,
    Spent,
    /// It overrode a session: stopped, interrupted, refused, changed its
    /// mode, or closed it.
    Overrode,
    /// It set something to happen later ("merge it once CI is green").
    Later,
}

impl Important {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Released => "released",
            Self::Deleted => "deleted",
            Self::Spent => "spent",
            Self::Overrode => "overrode",
            Self::Later => "later",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "merged" => Self::Merged,
            "released" => Self::Released,
            "deleted" => Self::Deleted,
            "spent" => Self::Spent,
            "overrode" => Self::Overrode,
            "later" => Self::Later,
            _ => return None,
        })
    }
}

/// What a crew action was, as far as its importance goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// A message to a session or an agent, or an answer to its question.
    Message,
    /// It allowed a session's request.
    Approve,
    /// It refused a session's request (not a plan sent back for changes).
    Deny,
    /// It stopped, interrupted or closed a session, or changed its mode.
    Override,
    /// Anything else: started, resumed, restarted, assigned, moved a
    /// ticket, took on or let go.
    Other,
}

const MERGE: &[&str] = &["merge", "merged", "merges", "merging"];
const RELEASE: &[&str] = &[
    "release",
    "released",
    "releases",
    "releasing",
    "deploy",
    "deployed",
    "deploys",
    "deploying",
    "ship",
    "ships",
    "shipped",
    "publish",
    "published",
    "promote",
    "promoted",
    "tag",
    "tagged",
];
const DELETE: &[&str] = &[
    "delete", "deleted", "deletes", "deleting", "remove", "removed", "rm", "drop", "dropped",
    "wipe", "wiped", "prune", "pruned", "purge", "purged", "clear", "clears", "cleared",
];
const SPEND: &[&str] = &[
    "spend", "spent", "pay", "paid", "buy", "bought", "purchase", "budget",
];
/// Words that say a message holds the step back rather than takes it.
const HOLD: &[&str] = &[
    "wait", "hold", "don't", "dont", "do not", "not yet", "never", "before", "until", "pause",
    "stop", "without",
];
/// Words that say a step is set to happen later.
const LATER: &[&str] = &[
    "if",
    "once",
    "when",
    "after",
    "tonight",
    "tomorrow",
    "later",
    "schedule",
    "scheduled",
];

/// The words of `text`, in lower case, apostrophes kept.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '’')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase().replace('’', "'"))
        .collect()
}

/// Whether `w` holds any of `list` (a two-word entry as a pair).
fn says(w: &[String], list: &[&str]) -> bool {
    list.iter().any(|l| match l.split_once(' ') {
        Some((a, b)) => w.windows(2).any(|p| p[0] == a && p[1] == b),
        None => w.iter().any(|x| x == l),
    })
}

/// Whether a decision is important, and why: from what kind of action it
/// was and what it said (`text`: the request it allowed, or the message it
/// sent). Overriding a session always is; allowing a request is when the
/// request merges, releases, deletes or spends; a message is when it tells
/// the session to do one of those, now or later, never when it holds it
/// back ("wait for green CI before merging").
pub fn important(kind: ActionKind, text: &str) -> Option<Important> {
    let w = words(text);
    let verb = || {
        [
            (MERGE, Important::Merged),
            (RELEASE, Important::Released),
            (DELETE, Important::Deleted),
            (SPEND, Important::Spent),
        ]
        .into_iter()
        .find(|(list, _)| says(&w, list))
        .map(|(_, i)| i)
    };
    match kind {
        ActionKind::Override | ActionKind::Deny => Some(Important::Overrode),
        ActionKind::Approve => verb(),
        ActionKind::Message => {
            let v = verb()?;
            if says(&w, HOLD) {
                return None;
            }
            Some(match says(&w, LATER) {
                true => Important::Later,
                false => v,
            })
        }
        ActionKind::Other => None,
    }
}

/// Where an event of `kind` goes by `c`, `quiet` being whether it is the
/// quiet hours: in them, what would come right away waits for the roundup,
/// except what needs the person when they said so. Needs-you is never only
/// on asking.
pub fn route(kind: EventKind, c: &Cadence, quiet: bool) -> Route {
    let base = match kind {
        EventKind::NeedsYou => match c.needs_you {
            Route::Ask => Route::RightAway,
            r => r,
        },
        EventKind::Problem => c.problems,
        EventKind::Finished => c.finished,
        EventKind::Progress => c.progress,
        EventKind::Routine => c.routine,
    };
    match (base, quiet) {
        (Route::RightAway, true) if !(kind == EventKind::NeedsYou && c.quiet_needs_you) => {
            Route::Roundup
        }
        (r, _) => r,
    }
}

/// Where a decision goes by `c`: an important one by the decisions
/// setting, any other as routine. In the quiet hours, as [`route`].
pub fn decision_route(important: bool, c: &Cadence, quiet: bool) -> Route {
    let r = match (c.decisions, important) {
        (Decided::Ask, _) => Route::Ask,
        (Decided::All, _) => Route::Roundup,
        (Decided::Important, true) => Route::Roundup,
        (Decided::ImportantRightAway, true) => Route::RightAway,
        (_, false) => c.routine,
    };
    match (r, quiet) {
        (Route::RightAway, true) => Route::Roundup,
        (r, _) => r,
    }
}

/// Whether a decision is listed under "Decided without you" in a
/// roundup: an important one unless the person hears of them only on
/// asking, any when they asked for every one.
pub fn listed(important: bool, c: &Cadence) -> bool {
    match c.decisions {
        Decided::Ask => false,
        Decided::All => true,
        Decided::Important | Decided::ImportantRightAway => important,
    }
}

/// Whether `at` (local time) is in the quiet hours `quiet` (from, until),
/// which may run over midnight: from is in them, until isn't.
pub fn in_quiet(quiet: Option<((u32, u32), (u32, u32))>, at: NaiveTime) -> bool {
    let Some(((fh, fm), (uh, um))) = quiet else {
        return false;
    };
    let m = at.hour() * 60 + at.minute();
    let (from, until) = (fh * 60 + fm, uh * 60 + um);
    match from < until {
        true => (from..until).contains(&m),
        // Over midnight: 22:00–07:00.
        false => m >= from || m < until,
    }
}

/// Whether it is the quiet hours of `c` at `now`, in `tz`.
pub fn quiet_now<Tz: TimeZone>(c: &Cadence, now: DateTime<Utc>, tz: &Tz) -> bool {
    in_quiet(c.quiet_hours, now.with_timezone(tz).time())
}

/// The first roundup time of `c` after `after`, in `tz`: on the clock
/// (every 30 minutes at :00 and :30, every hour at :00, every two hours at
/// even hours) or at morning, lunch and end of day, never in the quiet
/// hours; none when roundups never come.
pub fn next_roundup<Tz: TimeZone>(
    c: &Cadence,
    after: DateTime<Utc>,
    tz: &Tz,
) -> Option<DateTime<Utc>> {
    let step = match c.roundup {
        RoundupEvery::Never => return None,
        RoundupEvery::Minutes(m) => m.clamp(30, 120),
        // The meal times are on the half hour.
        RoundupEvery::Meals => 30,
    };
    let local = after.with_timezone(tz);
    let minutes = local.hour() * 60 + local.minute();
    // The next slot of `step` minutes on the clock, strictly after.
    let first = (minutes / step + 1) * step;
    let midnight = local
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|m| tz.from_local_datetime(&m).earliest())?
        .with_timezone(&Utc);
    // Two days of slots is always enough to find one out of quiet hours.
    for k in 0..(2 * 24 * 60 / step + 2) {
        let at = midnight + Duration::minutes(i64::from(first + k * step));
        if at <= after {
            continue;
        }
        let t = at.with_timezone(tz).time();
        let on = match c.roundup {
            RoundupEvery::Meals => RoundupEvery::MEALS
                .iter()
                .any(|(h, m)| t.hour() == *h && t.minute() == *m),
            _ => true,
        };
        if on && !in_quiet(c.quiet_hours, t) {
            return Some(at);
        }
    }
    None
}

/// Until when what needs the person waits by `c` at `now`, in `tz`: none
/// when it comes right away; else the next roundup, or, with none to come,
/// the end of the quiet hours.
pub fn held_until<Tz: TimeZone>(c: &Cadence, now: DateTime<Utc>, tz: &Tz) -> Option<DateTime<Utc>> {
    if route(EventKind::NeedsYou, c, quiet_now(c, now, tz)) == Route::RightAway {
        return None;
    }
    next_roundup(c, now, tz).or_else(|| {
        let every_half_hour = Cadence {
            roundup: RoundupEvery::Minutes(30),
            ..*c
        };
        next_roundup(&every_half_hour, now, tz)
    })
}

/// Whether a roundup is due at `now`: the first roundup time after the
/// last one (`last`) has come.
pub fn roundup_due<Tz: TimeZone>(
    c: &Cadence,
    last: DateTime<Utc>,
    now: DateTime<Utc>,
    tz: &Tz,
) -> bool {
    next_roundup(c, last, tz).is_some_and(|at| at <= now)
}

/// A roundup's sections, in the order it says them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SectionKey {
    UpToYou,
    Decided,
    Finished,
    WorthKnowing,
    StillGoing,
}

impl SectionKey {
    pub const ORDER: [Self; 5] = [
        Self::UpToYou,
        Self::Decided,
        Self::Finished,
        Self::WorthKnowing,
        Self::StillGoing,
    ];

    /// Its title, as the roundup says it.
    pub fn title(self) -> &'static str {
        match self {
            Self::UpToYou => "Up to you",
            Self::Decided => "Decided without you",
            Self::Finished => "Finished",
            Self::WorthKnowing => "Worth knowing",
            Self::StillGoing => "Still going",
        }
    }

    /// The section an item of `kind` is said in.
    pub fn of(kind: EventKind) -> Self {
        match kind {
            EventKind::NeedsYou => Self::UpToYou,
            EventKind::Finished => Self::Finished,
            EventKind::Problem => Self::WorthKnowing,
            EventKind::Progress | EventKind::Routine => Self::StillGoing,
        }
    }
}

/// One section of a roundup: its lines, each a full sentence with its
/// session's link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    pub key: SectionKey,
    pub title: String,
    pub lines: Vec<Said>,
}

/// A roundup (shape C, `extras.update.roundup`): what happened since the
/// last, by what it means to the person, each section only when it says
/// something, and how many routine things it left out ("N routine things
/// · Show").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Roundup {
    pub since: DateTime<Utc>,
    pub sections: Vec<Section>,
    #[serde(default)]
    pub routine: u32,
}

impl Roundup {
    /// `lines` (each with the section it goes in) as a roundup since
    /// `since`, its sections in order and each only when it has a line.
    pub fn of(since: DateTime<Utc>, lines: Vec<(SectionKey, Said)>, routine: u32) -> Self {
        let sections = SectionKey::ORDER
            .into_iter()
            .map(|key| Section {
                key,
                title: key.title().to_string(),
                lines: lines
                    .iter()
                    .filter(|(k, _)| *k == key)
                    .map(|(_, s)| s.clone())
                    .collect(),
            })
            .filter(|s| !s.lines.is_empty())
            .collect();
        Self {
            since,
            sections,
            routine,
        }
    }

    /// Whether it says nothing but routine: then it isn't sent.
    pub fn is_empty(&self) -> bool {
        self.sections.iter().all(|s| s.lines.is_empty())
    }

    /// Its one-line headline, written plainly: what is in it, in counts.
    pub fn plain_headline(&self) -> String {
        let n = |k: SectionKey| {
            self.sections
                .iter()
                .find(|s| s.key == k)
                .map_or(0, |s| s.lines.len())
        };
        let mut parts = Vec::new();
        let (fin, up, dec, worth, going) = (
            n(SectionKey::Finished),
            n(SectionKey::UpToYou),
            n(SectionKey::Decided),
            n(SectionKey::WorthKnowing),
            n(SectionKey::StillGoing),
        );
        if fin > 0 {
            parts.push(format!("{fin} finished"));
        }
        if up > 0 {
            parts.push(format!("{up} up to you"));
        }
        if dec > 0 {
            parts.push(format!("{dec} decided without you"));
        }
        if worth > 0 {
            parts.push(format!("{worth} worth knowing"));
        }
        if going > 0 {
            parts.push(format!("{going} still going"));
        }
        match parts.is_empty() {
            true => "Nothing new.".into(),
            false => crate::update_write::capitalised(&format!("{}.", parts.join(", "))),
        }
    }
}

/// A roundup as text, for history, Slack and the model: its headline, each
/// section's title and lines, and the routine count.
pub fn render(headline: &str, r: &Roundup) -> String {
    let mut out = vec![headline.trim().to_string()];
    for s in &r.sections {
        let lines: Vec<String> = s
            .lines
            .iter()
            .map(|l| format!("- {}", crate::update_write::render(l).replace('\n', " · ")))
            .collect();
        out.push(format!("**{}**\n{}", s.title, lines.join("\n")));
    }
    if r.routine > 0 {
        out.push(format!(
            "{} routine {}: ask \"what else did you decide?\"",
            r.routine,
            if r.routine == 1 { "thing" } else { "things" }
        ));
    }
    out.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update_write::SessionLink;

    fn balanced() -> Cadence {
        Cadence::default()
    }

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// Each kind goes where the person's cadence sends it; needs-you is
    /// never only on asking.
    #[test]
    fn each_kind_is_routed_as_the_cadence_says() {
        let c = balanced();
        assert_eq!(c.preset_name(), Some("balanced"));
        assert_eq!(route(EventKind::NeedsYou, &c, false), Route::RightAway);
        assert_eq!(route(EventKind::Problem, &c, false), Route::RightAway);
        assert_eq!(route(EventKind::Finished, &c, false), Route::Roundup);
        assert_eq!(route(EventKind::Progress, &c, false), Route::Roundup);
        assert_eq!(route(EventKind::Routine, &c, false), Route::Ask);

        let now = Cadence::preset("as_it_happens").unwrap();
        assert_eq!(route(EventKind::Finished, &now, false), Route::RightAway);
        assert_eq!(route(EventKind::Progress, &now, false), Route::RightAway);
        assert_eq!(route(EventKind::Routine, &now, false), Route::Ask);
        assert_eq!(now.roundup, RoundupEvery::Never);

        let later = Cadence::preset("roundups_only").unwrap();
        assert_eq!(route(EventKind::NeedsYou, &later, false), Route::RightAway);
        assert_eq!(route(EventKind::Problem, &later, false), Route::Roundup);

        let mut custom = c;
        custom.progress = Route::Ask;
        assert_eq!(custom.preset_name(), None, "a changed row is Custom");
        custom.needs_you = Route::Ask;
        assert_eq!(route(EventKind::NeedsYou, &custom, false), Route::RightAway);
    }

    /// Quiet hours hold what would come right away for the roundup, except
    /// what needs the person when they said so; they run over midnight.
    #[test]
    fn quiet_hours_hold_all_but_what_needs_you() {
        let c = balanced();
        assert!(in_quiet(c.quiet_hours, t(22, 0)));
        assert!(in_quiet(c.quiet_hours, t(3, 15)));
        assert!(in_quiet(c.quiet_hours, t(6, 59)));
        assert!(!in_quiet(c.quiet_hours, t(7, 0)));
        assert!(!in_quiet(c.quiet_hours, t(21, 59)));
        assert!(!in_quiet(None, t(3, 0)));
        assert!(in_quiet(Some(((12, 0), (13, 0))), t(12, 30)));
        assert!(!in_quiet(Some(((12, 0), (13, 0))), t(13, 0)));

        assert_eq!(route(EventKind::NeedsYou, &c, true), Route::RightAway);
        assert_eq!(route(EventKind::Problem, &c, true), Route::Roundup);
        let mut strict = c;
        strict.quiet_needs_you = false;
        assert_eq!(route(EventKind::NeedsYou, &strict, true), Route::Roundup);
        let mut now = Cadence::preset("as_it_happens").unwrap();
        now.decisions = Decided::ImportantRightAway;
        assert_eq!(decision_route(true, &now, false), Route::RightAway);
        assert_eq!(decision_route(true, &now, true), Route::Roundup);

        // What needs the person waits only when they said so.
        let utc = chrono_tz::UTC;
        assert_eq!(held_until(&c, at("2026-11-16T23:00:00Z"), &utc), None);
        assert_eq!(
            held_until(&strict, at("2026-11-16T23:00:00Z"), &utc),
            Some(at("2026-11-17T07:00:00Z"))
        );
        let mut never = strict;
        never.roundup = RoundupEvery::Never;
        assert_eq!(
            held_until(&never, at("2026-11-16T23:00:00Z"), &utc),
            Some(at("2026-11-17T07:00:00Z")),
            "no roundups: until the quiet hours end"
        );
        let tz = chrono_tz::Europe::London;
        // 23:00 in London in November: no quiet-hours roundups, the next
        // at 07:00, the first hour out of them.
        assert!(quiet_now(&c, at("2026-11-16T23:00:00Z"), &tz));
        assert_eq!(
            next_roundup(&c, at("2026-11-16T21:40:00Z"), &tz),
            Some(at("2026-11-17T07:00:00Z"))
        );
    }

    /// Which decisions are important: merging, releasing, deleting,
    /// spending, overriding a session, setting something for later; never
    /// a message that holds a step back, nor the routine.
    #[test]
    fn which_decisions_count_as_important() {
        use ActionKind::*;
        assert_eq!(
            important(Override, "stopped web"),
            Some(Important::Overrode)
        );
        assert_eq!(
            important(Deny, "refused the push"),
            Some(Important::Overrode)
        );
        assert_eq!(
            important(Approve, "Bash: gh pr merge 631 --squash"),
            Some(Important::Merged)
        );
        assert_eq!(
            important(Approve, "Bash: rm -rf target/"),
            Some(Important::Deleted)
        );
        assert_eq!(important(Approve, "Bash: cargo test"), None);
        assert_eq!(
            important(Message, "Merge both changes yourself."),
            Some(Important::Merged)
        );
        assert_eq!(
            important(
                Message,
                "Delete rebuildable build files on the devbox if free space falls under 50 GB."
            ),
            Some(Important::Later)
        );
        assert_eq!(
            important(Message, "Wait for green CI before merging."),
            None
        );
        assert_eq!(important(Message, "Don't deploy tonight."), None);
        assert_eq!(important(Message, "Run the tests again."), None);
        assert_eq!(important(Other, "restarted web"), None);

        let c = balanced();
        assert_eq!(decision_route(true, &c, false), Route::Roundup);
        assert_eq!(decision_route(false, &c, false), Route::Ask, "routine");
        assert!(listed(true, &c) && !listed(false, &c));
        let mut every = c;
        every.decisions = Decided::All;
        assert!(listed(false, &every));
        every.decisions = Decided::Ask;
        assert!(!listed(true, &every));
        assert_eq!(decision_route(true, &every, false), Route::Ask);
    }

    #[test]
    fn roundups_come_on_the_clock() {
        let tz = chrono_tz::UTC;
        let c = balanced();
        assert_eq!(
            next_roundup(&c, at("2026-11-16T09:00:00Z"), &tz),
            Some(at("2026-11-16T10:00:00Z"))
        );
        assert_eq!(
            next_roundup(&c, at("2026-11-16T09:59:00Z"), &tz),
            Some(at("2026-11-16T10:00:00Z"))
        );
        assert!(roundup_due(
            &c,
            at("2026-11-16T09:10:00Z"),
            at("2026-11-16T10:00:30Z"),
            &tz
        ));
        assert!(!roundup_due(
            &c,
            at("2026-11-16T09:10:00Z"),
            at("2026-11-16T09:59:30Z"),
            &tz
        ));
        let mut half = c;
        half.roundup = RoundupEvery::Minutes(30);
        assert_eq!(
            next_roundup(&half, at("2026-11-16T09:10:00Z"), &tz),
            Some(at("2026-11-16T09:30:00Z"))
        );
        let mut two = c;
        two.roundup = RoundupEvery::Minutes(120);
        assert_eq!(
            next_roundup(&two, at("2026-11-16T09:10:00Z"), &tz),
            Some(at("2026-11-16T10:00:00Z"))
        );
        let mut meals = c;
        meals.roundup = RoundupEvery::Meals;
        assert_eq!(
            next_roundup(&meals, at("2026-11-16T09:10:00Z"), &tz),
            Some(at("2026-11-16T12:30:00Z"))
        );
        assert_eq!(
            next_roundup(&meals, at("2026-11-16T17:30:00Z"), &tz),
            Some(at("2026-11-17T09:00:00Z"))
        );
        let mut never = c;
        never.roundup = RoundupEvery::Never;
        assert_eq!(next_roundup(&never, at("2026-11-16T09:10:00Z"), &tz), None);
    }

    fn line(kind: EventKind, headline: &str) -> Said {
        Said {
            kind,
            headline: headline.into(),
            body: String::new(),
            ids: vec![],
            not_checked: None,
            session: Some(SessionLink {
                session_id: "s".into(),
                name: "web".into(),
                closed: false,
            }),
            did: vec![],
            at: None,
        }
    }

    /// A roundup says its sections in order, each only when it has a
    /// line; one with only routine things is empty, and not sent.
    #[test]
    fn an_empty_roundup_is_skipped_and_sections_come_in_order() {
        let since = at("2026-11-16T08:00:00Z");
        let routine_only = Roundup::of(since, vec![], 15);
        assert!(routine_only.is_empty());
        let r = Roundup::of(
            since,
            vec![
                (
                    SectionKey::StillGoing,
                    line(EventKind::Progress, "Production is steady."),
                ),
                (
                    SectionKey::Finished,
                    line(EventKind::Finished, "The Rust builds work again."),
                ),
                (
                    SectionKey::UpToYou,
                    line(EventKind::NeedsYou, "Cufflink is paused."),
                ),
            ],
            15,
        );
        assert!(!r.is_empty());
        let keys: Vec<SectionKey> = r.sections.iter().map(|s| s.key).collect();
        assert_eq!(
            keys,
            vec![
                SectionKey::UpToYou,
                SectionKey::Finished,
                SectionKey::StillGoing
            ]
        );
        assert_eq!(r.sections[0].title, "Up to you");
        assert_eq!(
            r.plain_headline(),
            "1 finished, 1 up to you, 1 still going."
        );
        let text = render("Two fixes are finished.", &r);
        assert!(
            text.contains("**Finished**\n- **The Rust builds work again.**"),
            "{text}"
        );
        assert!(
            text.ends_with("15 routine things: ask \"what else did you decide?\""),
            "{text}"
        );
    }
}
