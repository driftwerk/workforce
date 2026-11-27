//! The person's plan limit as a plan meets it.
//!
//! The plan guard holds a boss's own wakes while the plan they run on is at
//! the person's limit. Each wake judged notes what the guard said of it
//! ([`Noted`]); a plan whose boss's wakes are held pauses (`plan_limit`),
//! the person is asked once, in plain words, whether to raise the limit,
//! and the plan goes on, said, once the limit no longer holds its boss.
//! Nothing is predicted: only a wake really held pauses a plan, and after a
//! restart (nothing noted) a paused plan waits for its window to lapse.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use workforce_desk::crew;
use workforce_desk::open_item::{ClosedBy, NewItem, OpenItem, OpenKind};
use workforce_desk::open_item_store::Opened;
use workforce_desk::plan::{self, Paused, Plan, PlanState};

use super::CrewHub;
use crate::cost_governor::Admission;
use crate::hub_core::logins::resolve::Login;
use crate::hub_core::plan_guard::{
    plan_words, points, window_words, Held, PlanStanding, WAITS_PREFIX,
};
use crate::serve::AgentService;

/// The answer that keeps the limit as it is.
const KEEP: &str = "Keep";
/// The limits offered above the one that holds, lowest first.
const RAISES: [u8; 2] = [95, 100];
/// How many points under the limit a boss is told its plans will pause.
const NEAR: f64 = 10.0;
/// Why the boss leaves the limit to the person, at any level.
const CANT_DECIDE: &str = "Only you can change your plan limit.";

/// How long a boss's own wake on a login counts for whether the limit
/// holds it.
fn recent() -> Duration {
    Duration::hours(1)
}

/// What the plan guard said of the bosses' own wakes, login by login, and
/// the pauses whose end was said.
#[derive(Default)]
pub(super) struct Noted {
    wakes: HashMap<String, Vec<Wake>>,
    resumed: HashSet<String>,
}

/// One of a boss's own wakes as the plan guard judged it: the plan login
/// it ran on (none: not a plan), whether the limit held it, and when.
#[derive(Clone)]
struct Wake {
    login: Option<Login>,
    held: bool,
    at: DateTime<Utc>,
}

impl Wake {
    fn same_login(&self, other: &Wake) -> bool {
        self.login.as_ref().map(|l| &l.key) == other.login.as_ref().map(|l| &l.key)
    }
}

/// Whether the person's plan limit holds a boss's own wakes now.
enum Limit {
    Holds(Held),
    /// It doesn't: where the plan its latest wake on one stands, if read.
    Lifted(Option<PlanStanding>),
    /// Nothing was noted of its wakes lately.
    Unknown,
}

/// The person's answer to a plan-limit ask.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Answer {
    Raise(u8),
    Keep,
}

/// What the person's answer to a plan-limit ask did to their limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Kept {
    Raised(u8),
    /// Raised to no more than it already is: nothing changed.
    Already(f64),
    Unchanged,
}

fn raise_label(to: u8) -> String {
    format!("Raise to {to}%")
}

/// The lowest limit offered that would let `held` go on.
fn raise_to(held: &Held) -> Option<u8> {
    RAISES.into_iter().find(|r| {
        let r = f64::from(*r);
        r > held.standing.used_percent && r > held.threshold
    })
}

/// What `answer` to `item` says, when `item` asks about a plan-limit pause
/// and `answer` is one of its options.
fn answer_of(item: &OpenItem, answer: &str) -> Option<Answer> {
    plan::limit_plan(&item.dedupe_key)?;
    let offered = item
        .options
        .iter()
        .map(|o| o.trim())
        .find(|o| o.eq_ignore_ascii_case(answer.trim()))?;
    if offered.eq_ignore_ascii_case(KEEP) {
        return Some(Answer::Keep);
    }
    RAISES
        .into_iter()
        .find(|r| raise_label(*r).eq_ignore_ascii_case(offered))
        .map(Answer::Raise)
}

/// Whether `answer` to `item` is the person's word on a plan-limit pause,
/// which Desk acts on, never the boss.
pub(super) fn answers(item: &OpenItem, answer: &str) -> bool {
    answer_of(item, answer).is_some()
}

/// "09:00" today, "Fri 09:00" within the week, else "Fri 4 Dec 09:00", in
/// the person's time.
fn at_time(t: DateTime<Utc>, now: DateTime<Utc>, tz: chrono_tz::Tz) -> String {
    let (local, today) = (t.with_timezone(&tz), now.with_timezone(&tz));
    let days = (local.date_naive() - today.date_naive()).num_days();
    let format = match days {
        0 => "%H:%M",
        1..=6 => "%a %H:%M",
        _ => "%a %-d %b %H:%M",
    };
    local.format(format).to_string()
}

/// A plan's goal as it is named to the person.
fn named(goal: &str) -> String {
    format!("“{}”", crew::one_line(goal, 80))
}

/// "your weekly ChatGPT plan is at 88%, over your 80% limit".
fn over(held: &Held) -> String {
    format!(
        "{}, over your {} limit",
        held.standing.says(),
        points(held.threshold)
    )
}

/// Why the plan is paused, after "Paused " on its card and the Floor.
fn paused_words(held: &Held, now: DateTime<Utc>, tz: chrono_tz::Tz) -> String {
    format!(
        "by plan limit until {}: {}",
        at_time(held.until, now, tz),
        over(held)
    )
}

/// What the person is told when the limit first pauses `goal`.
fn told_words(
    goal: &str,
    held: &Held,
    raise: bool,
    now: DateTime<Utc>,
    tz: chrono_tz::Tz,
) -> String {
    format!(
        "{} is paused: {}. Nothing will move until {}{}.",
        named(goal),
        over(held),
        at_time(held.until, now, tz),
        if raise {
            " unless you raise the limit"
        } else {
            ""
        }
    )
}

/// What the person is told when `goal` goes on: by where its boss's plan
/// stands now, else by its window having reset at `until`.
fn resumed_words(
    goal: &str,
    standing: Option<&PlanStanding>,
    threshold: Option<f64>,
    until: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    tz: chrono_tz::Tz,
) -> String {
    let why = match (standing, threshold, until) {
        (Some(s), Some(t), _) if s.used_percent < t => {
            format!("{}, under your {} limit", s.says(), points(t))
        }
        (_, _, Some(until)) if until <= now => {
            format!("its plan's window reset at {}", at_time(until, now, tz))
        }
        _ => "the plan limit no longer holds its boss".into(),
    };
    format!("{} goes on: {why}.", named(goal))
}

/// What a boss is told of its own wakes' plan in every turn: held now, or
/// near the limit.
fn limit_words(
    held: Option<&Held>,
    standing: Option<&PlanStanding>,
    threshold: f64,
    now: DateTime<Utc>,
    tz: chrono_tz::Tz,
) -> Option<String> {
    if let Some(h) = held {
        return Some(format!(
            "Your own turns (check-ins, notices, wake-ups) are paused now: {}, over the person's {} limit, until {}. Your plans are paused with them. If the person asks you to keep one going, tell them plainly it can't until then unless they raise the limit; only they can.",
            h.standing.says(),
            points(h.threshold),
            at_time(h.until, now, tz)
        ));
    }
    let s = standing.filter(|s| s.used_percent >= threshold - NEAR)?;
    let until = s
        .resets_at
        .map_or_else(String::new, |r| format!(" at {}", at_time(r, now, tz)));
    let at = points(threshold);
    Some(format!(
        "Your own turns (check-ins, notices, wake-ups) run on your {} {} plan, now at {}. They pause from {at}, the person's limit, and your plans pause with them until the window resets{until}. If the person asks you to keep a plan going, or to watch one while they are away, tell them plainly it will pause at {at} unless they raise the limit; only they can.",
        window_words(s.window_minutes),
        plan_words(s.plan),
        points(s.used_percent)
    ))
}

impl CrewHub {
    /// Notes what the plan guard said of `service`'s own wake on `login`
    /// at `now` (`admitted`): held by the limit, or run on a plan read under
    /// it (or on none). A plan not read yet says nothing.
    pub(super) fn note_wake(
        &self,
        service: &AgentService,
        login: Option<Login>,
        admitted: &Admission,
        now: DateTime<Utc>,
    ) {
        let held = matches!(admitted, Admission::Waits(why) if why.starts_with(WAITS_PREFIX));
        let read = login.as_ref().is_none_or(|l| {
            held || service
                .cost_governor
                .plan_guard()
                .standing(l, now)
                .is_some()
        });
        if !read {
            return;
        }
        let wake = Wake {
            login,
            held,
            at: now,
        };
        let mut noted = self.limits.lock();
        let wakes = noted
            .wakes
            .entry(service.agent_id.as_str().to_string())
            .or_default();
        wakes.retain(|w| w.at > now - recent() && !w.same_login(&wake));
        wakes.push(wake);
    }

    /// Whether the limit holds `boss` now: while any plan login its recent
    /// own wakes were held on still is.
    fn limit_of(&self, boss: &str, now: DateTime<Utc>) -> Limit {
        let Some((service, _)) = self.overseer_named(boss) else {
            return Limit::Unknown;
        };
        let wakes: Vec<Wake> = self
            .limits
            .lock()
            .wakes
            .get(boss)
            .map(|w| {
                w.iter()
                    .filter(|w| w.at > now - recent())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let governor = &service.cost_governor;
        let held = wakes
            .iter()
            .filter(|w| w.held)
            .filter_map(|w| governor.plan_holds(w.login.as_ref()?, now))
            .max_by(|a, b| a.standing.used_percent.total_cmp(&b.standing.used_percent));
        match held {
            Some(h) => Limit::Holds(h),
            None if wakes.is_empty() => Limit::Unknown,
            None => Limit::Lifted(
                wakes
                    .iter()
                    .filter_map(|w| Some((w.at, w.login.as_ref()?)))
                    .max_by_key(|(at, _)| *at)
                    .and_then(|(_, l)| governor.plan_guard().standing(l, now)),
            ),
        }
    }

    /// Whether the person's plan limit holds `p`'s boss: then `p` is paused
    /// for it, and the person asked once.
    pub(super) async fn held_by_limit(&self, p: &Plan) -> bool {
        let now = Utc::now();
        let Limit::Holds(held) = self.limit_of(&p.boss, now) else {
            return false;
        };
        let words = paused_words(&held, now, self.desk.timezone());
        match self.desk.pause_plan_limit(p.id, &words, held.until).await {
            Ok(_) => self.ask_limit(p, &held, now).await,
            Err(e) => {
                tracing::info!(plan = %p.id, error = %e, "a plan the plan limit holds wasn't paused")
            }
        }
        true
    }

    /// Asks the person, once for `p` and the window holding it, whether
    /// to raise the limit; an ask of an earlier window is closed then.
    async fn ask_limit(&self, p: &Plan, held: &Held, now: DateTime<Utc>) {
        let store = self.desk.store();
        let key = plan::limit_key(p.id, held.until);
        match store.item_ever_keyed(&p.boss, &key).await {
            Ok(false) => {}
            Ok(true) => return,
            Err(e) => {
                tracing::info!(error = %e, "whether a plan's limit pause was said couldn't be read");
                return;
            }
        }
        if let Err(e) = store
            .item_close_of_key_prefix(
                &plan::limit_prefix(p.id),
                ClosedBy::Lapsed,
                "A newer window holds it.",
            )
            .await
        {
            tracing::info!(plan = %p.id, error = %e, "a plan's earlier limit asks weren't closed");
        }
        let raise = raise_to(held);
        let text = told_words(&p.goal, held, raise.is_some(), now, self.desk.timezone());
        let mut new = NewItem::ask(&p.boss, OpenKind::Ask, None, &text);
        new.dedupe_key = key;
        new.options = raise
            .map(raise_label)
            .into_iter()
            .chain([KEEP.to_string()])
            .collect();
        new.cant_decide = Some(CANT_DECIDE.into());
        new.holding_up = Some(format!("{} and its tickets", named(&p.goal)));
        match store.item_open(&new).await {
            Ok(Opened::New(i)) => {
                self.post_item(&p.boss, &i, super::items::needs_you(&i), Vec::new())
                    .await;
            }
            Ok(Opened::Again(_)) => {}
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "asking the person about a plan's limit pause failed")
            }
        }
    }

    /// Resumes `p`, paused by the plan limit, once that no longer holds its
    /// boss (or, with nothing noted lately, once its window lapsed), and
    /// says so once a pause; while it holds, its words are kept current and
    /// the person asked of each window.
    pub(super) async fn lift_limit(&self, p: &Plan) {
        let now = Utc::now();
        let tz = self.desk.timezone();
        let standing = match self.limit_of(&p.boss, now) {
            Limit::Holds(held) => {
                let words = paused_words(&held, now, tz);
                let current =
                    p.paused_why.as_deref() == Some(&words) && p.paused_until == Some(held.until);
                if !current {
                    if let Err(e) = self.desk.pause_plan_limit(p.id, &words, held.until).await {
                        tracing::info!(plan = %p.id, error = %e, "a plan's limit pause wasn't kept current");
                    }
                }
                self.ask_limit(p, &held, now).await;
                return;
            }
            Limit::Unknown if p.paused_until.is_some_and(|u| u > now) => return,
            Limit::Unknown => None,
            Limit::Lifted(standing) => standing,
        };
        match self.desk.resume_plan_limit(p.id).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                tracing::info!(plan = %p.id, error = %e, "a plan the plan limit let go of wasn't resumed");
                return;
            }
        }
        if let Err(e) = self
            .desk
            .store()
            .item_close_of_key_prefix(
                &plan::limit_prefix(p.id),
                ClosedBy::Lapsed,
                "The plan limit no longer holds it.",
            )
            .await
        {
            tracing::info!(plan = %p.id, error = %e, "a plan's limit asks weren't closed");
        }
        let pause = plan::limit_key(p.id, p.paused_until.unwrap_or(now));
        if !self.limits.lock().resumed.insert(pause) {
            return;
        }
        let threshold = self
            .overseer_named(&p.boss)
            .and_then(|(s, _)| s.cost_governor.plan_threshold());
        let text = resumed_words(
            &p.goal,
            standing.as_ref(),
            threshold,
            p.paused_until,
            now,
            tz,
        );
        self.tell_dm(&p.boss, &text).await;
    }

    /// Every plan the plan limit pauses, lifted where it no longer holds.
    async fn lift_limits(&self) {
        match self.desk.plans(Some(PlanState::Paused), None).await {
            Ok(paused) => {
                for s in paused
                    .into_iter()
                    .filter(|s| s.plan.paused == Some(Paused::PlanLimit))
                {
                    self.lift_limit(&s.plan).await;
                }
            }
            Err(e) => tracing::warn!(error = %e, "couldn't read the plans the plan limit pauses"),
        }
    }

    /// What the person's `answer` to `item`, a plan-limit ask, does to
    /// their limit, kept before the ask closes: none when it asks nothing
    /// of the plan limit, refused when the limit couldn't be kept.
    pub(super) async fn keep_plan_limit(
        &self,
        item: &OpenItem,
        answer: &str,
    ) -> Option<Result<Kept, String>> {
        let to = match answer_of(item, answer)? {
            Answer::Keep => return Some(Ok(Kept::Unchanged)),
            Answer::Raise(to) => to,
        };
        let threshold = self
            .overseer_named(&item.boss)
            .and_then(|(s, _)| s.cost_governor.plan_threshold());
        if let Some(t) = threshold.filter(|t| *t >= f64::from(to)) {
            return Some(Ok(Kept::Already(t)));
        }
        Some(match self.desk.set_plan_pause_at(f64::from(to)).await {
            Ok(_) => Ok(Kept::Raised(to)),
            Err(e) => {
                tracing::warn!(error = %e, "the person's plan limit wasn't kept");
                Err("Desk couldn't change your limit just now. Try again.".into())
            }
        })
    }

    /// What the person is told of `kept`, their answer to `closed`: a raise
    /// lifts what it lets go at once.
    pub(super) async fn plan_limit_answered(&self, closed: &OpenItem, kept: Kept) -> String {
        match kept {
            Kept::Raised(to) => {
                self.lift_limits().await;
                format!(
                    "Raised: background work on your plans now pauses from {}.",
                    points(f64::from(to))
                )
            }
            Kept::Already(t) => format!("Your limit is already {}: nothing changed.", points(t)),
            Kept::Unchanged => {
                let threshold = self
                    .overseer_named(&closed.boss)
                    .and_then(|(s, _)| s.cost_governor.plan_threshold());
                let plan = match plan::limit_plan(&closed.dedupe_key) {
                    Some(id) => self.desk.store().plan(id).await.ok().flatten(),
                    None => None,
                };
                let kept = threshold.map_or_else(
                    || "Kept your limit.".to_string(),
                    |t| format!("Kept at {}.", points(t)),
                );
                let after = plan
                    .filter(|p| p.paused == Some(Paused::PlanLimit))
                    .and_then(|p| Some((p.paused_until?, p.goal)))
                    .map(|(until, goal)| {
                        format!(
                            " {} goes on by itself after {}.",
                            named(&goal),
                            at_time(until, Utc::now(), self.desk.timezone())
                        )
                    })
                    .unwrap_or_default();
                format!("{kept}{after}")
            }
        }
    }

    /// Whether `i`, a plan-limit ask, is past: its plan no longer paused
    /// by the limit (resumed, dropped, done, or gone).
    pub(super) async fn limit_ask_past(&self, i: &OpenItem) -> Option<Option<(ClosedBy, String)>> {
        let id = plan::limit_plan(&i.dedupe_key)?;
        Some(match self.desk.store().plan(id).await {
            Ok(Some(p)) if p.paused == Some(Paused::PlanLimit) => None,
            Ok(Some(p)) => Some((
                ClosedBy::Lapsed,
                format!(
                    "{} isn't paused by the plan limit any more.",
                    named(&p.goal)
                ),
            )),
            Ok(None) => Some((ClosedBy::Lapsed, "Its plan is gone.".into())),
            Err(e) => {
                tracing::info!(error = %e, "a plan-limit ask's plan couldn't be read");
                None
            }
        })
    }

    /// What `service`, a boss, is told in every turn of the plan its own
    /// wakes run on, when the limit holds them or it is near the limit.
    pub(super) fn plan_limit_line(&self, service: &AgentService) -> Option<String> {
        let threshold = service.cost_governor.plan_threshold()?;
        let now = Utc::now();
        let tz = self.desk.timezone();
        match self.limit_of(service.agent_id.as_str(), now) {
            Limit::Holds(h) => limit_words(Some(&h), None, threshold, now, tz),
            Limit::Lifted(standing) => limit_words(None, standing.as_ref(), threshold, now, tz),
            Limit::Unknown => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::usage::PlanKind;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    fn standing(used: f64, resets: &str) -> PlanStanding {
        PlanStanding {
            plan: PlanKind::Chatgpt,
            used_percent: used,
            window_minutes: 10_080,
            resets_at: Some(at(resets)),
        }
    }

    fn held(used: f64, threshold: f64) -> Held {
        Held {
            standing: standing(used, "2026-11-27T09:00:00Z"),
            threshold,
            until: at("2026-11-27T09:00:00Z"),
        }
    }

    fn ask(key: &str, options: &[&str]) -> OpenItem {
        let now = Utc::now();
        OpenItem {
            id: uuid::Uuid::new_v4(),
            boss: "sarah".into(),
            owner: "you".into(),
            kind: OpenKind::Ask,
            session: None,
            line_message: None,
            text: "“Eden” is paused".into(),
            holding_up: None,
            recommendation: None,
            options: options.iter().map(|o| o.to_string()).collect(),
            generalizes: None,
            done_when: None,
            dedupe_key: key.into(),
            opened_at: now,
            last_said_at: None,
            said_count: 0,
            raised_at: None,
            sessions: vec![],
            prefer_scope: None,
            changed_at: now,
            state: workforce_desk::open_item::OpenState::Open,
            closed_at: None,
            closed_by: None,
            closed_why: None,
            layout: Default::default(),
            cant_decide: None,
            due_at: None,
            chased_at: None,
            delivered_at: None,
        }
    }

    const NOW: &str = "2026-11-24T19:42:00Z";
    const LONDON: chrono_tz::Tz = chrono_tz::Europe::London;

    #[test]
    fn the_raise_offered_is_the_first_that_lets_the_plan_go_on() {
        assert_eq!(raise_to(&held(88.0, 80.0)), Some(95));
        assert_eq!(raise_to(&held(96.0, 80.0)), Some(100));
        assert_eq!(raise_to(&held(97.0, 95.0)), Some(100));
        assert_eq!(raise_to(&held(100.0, 80.0)), None);
        assert_eq!(raise_to(&held(85.0, 95.0)), Some(100));
    }

    #[test]
    fn only_an_option_a_plan_limit_ask_offers_is_read_as_its_answer() {
        let key = plan::limit_key(uuid::Uuid::new_v4(), at(NOW));
        let offers = ask(&key, &["Raise to 95%", "Keep"]);
        assert_eq!(answer_of(&offers, "Raise to 95%"), Some(Answer::Raise(95)));
        assert_eq!(
            answer_of(&offers, " raise to 95% "),
            Some(Answer::Raise(95))
        );
        assert_eq!(answer_of(&offers, "keep"), Some(Answer::Keep));
        assert_eq!(answer_of(&offers, "Raise to 100%"), None, "not offered");
        assert_eq!(answer_of(&offers, "Raise it a bit"), None);
        assert_eq!(answer_of(&ask(&key, &["Keep"]), "Raise to 95%"), None);
        assert_eq!(
            answer_of(&ask("plan-stopped:a:b", &["Keep"]), "Keep"),
            None,
            "not a plan-limit ask"
        );
        assert!(answers(&offers, "Keep"));
    }

    #[test]
    fn times_are_said_in_the_persons_zone_by_how_far_off_they_are() {
        let now = at(NOW);
        assert_eq!(at_time(at("2026-11-24T22:00:00Z"), now, LONDON), "22:00");
        assert_eq!(
            at_time(at("2026-11-25T08:25:00Z"), now, LONDON),
            "Wed 08:25"
        );
        assert_eq!(
            at_time(at("2026-12-01T19:42:00Z"), now, LONDON),
            "Tue 1 Dec 19:42"
        );
        assert_eq!(
            at_time(at("2026-11-24T23:30:00Z"), now, chrono_tz::Asia::Tokyo),
            "08:30"
        );
    }

    #[test]
    fn the_pause_is_said_plainly_on_the_card_and_to_the_person() {
        let now = at(NOW);
        let h = held(88.0, 80.0);
        assert_eq!(
            paused_words(&h, now, LONDON),
            "by plan limit until Fri 09:00: your weekly ChatGPT plan is at 88%, over your 80% limit"
        );
        assert_eq!(
            told_words("Eden", &h, true, now, LONDON),
            "“Eden” is paused: your weekly ChatGPT plan is at 88%, over your 80% limit. Nothing will move until Fri 09:00 unless you raise the limit."
        );
        assert_eq!(
            told_words("Eden", &held(100.0, 80.0), false, now, LONDON),
            "“Eden” is paused: your weekly ChatGPT plan is at 100%, over your 80% limit. Nothing will move until Fri 09:00."
        );
        assert_eq!(
            over(&held(88.6, 82.5)),
            "your weekly ChatGPT plan is at 88%, over your 82% limit",
            "every share in whole points"
        );
    }

    #[test]
    fn going_on_says_why() {
        let now = at("2026-11-27T10:00:00Z");
        let under = standing(12.0, "2026-12-04T09:00:00Z");
        assert_eq!(
            resumed_words("Eden", Some(&under), Some(80.0), None, now, LONDON),
            "“Eden” goes on: your weekly ChatGPT plan is at 12%, under your 80% limit."
        );
        let raised = standing(88.0, "2026-11-27T09:00:00Z");
        assert_eq!(
            resumed_words("Eden", Some(&raised), Some(95.0), None, now, LONDON),
            "“Eden” goes on: your weekly ChatGPT plan is at 88%, under your 95% limit."
        );
        assert_eq!(
            resumed_words(
                "Eden",
                None,
                Some(80.0),
                Some(at("2026-11-27T09:00:00Z")),
                now,
                LONDON
            ),
            "“Eden” goes on: its plan's window reset at 09:00."
        );
        assert_eq!(
            resumed_words("Eden", None, Some(80.0), None, now, LONDON),
            "“Eden” goes on: the plan limit no longer holds its boss."
        );
    }

    #[test]
    fn a_boss_is_told_of_its_plan_near_the_limit_and_while_held() {
        let now = at(NOW);
        let near = limit_words(
            None,
            Some(&standing(72.0, "2026-11-27T09:00:00Z")),
            80.0,
            now,
            LONDON,
        )
        .unwrap();
        assert!(
            near.contains("run on your weekly ChatGPT plan, now at 72%. They pause from 80%"),
            "{near}"
        );
        assert!(
            near.contains("will pause at 80% unless they raise the limit"),
            "{near}"
        );
        assert!(near.contains("resets at Fri 09:00"), "{near}");
        assert_eq!(
            limit_words(
                None,
                Some(&standing(60.0, "2026-11-27T09:00:00Z")),
                80.0,
                now,
                LONDON
            ),
            None
        );
        assert_eq!(limit_words(None, None, 80.0, now, LONDON), None);
        let h = held(88.0, 80.0);
        let paused = limit_words(Some(&h), None, 80.0, now, LONDON).unwrap();
        assert!(paused.contains("are paused now: your weekly ChatGPT plan is at 88%, over the person's 80% limit, until Fri 09:00"), "{paused}");
        assert!(
            paused.contains("it can't until then unless they raise the limit"),
            "{paused}"
        );
    }
}
