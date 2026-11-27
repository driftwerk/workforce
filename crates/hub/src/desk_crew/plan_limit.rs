//! The person's plan limit as a plan meets it (DW-760, the Eden plan, 24 Nov:
//! its boss's notices held 134 times overnight at 88% of a ChatGPT week,
//! the person told nothing, 3 of 14 tickets done).
//!
//! The plan guard holds a boss's own wakes while the plan they run on is at
//! the person's limit. Each wake judged notes what the guard said of it
//! ([`Wake`]); a plan whose boss's wakes are held pauses (`plan_limit`),
//! the person is asked once, in plain words, whether to raise the limit,
//! and the plan goes on, said, once the limit no longer holds its boss.
//! Nothing is predicted: only a wake really held pauses a plan, and after a
//! restart (nothing noted) a paused plan waits for its window to lapse.

use chrono::{DateTime, Utc};
use workforce_desk::crew;
use workforce_desk::open_item::{ClosedBy, NewItem, OpenItem, OpenKind};
use workforce_desk::open_item_store::Opened;
use workforce_desk::plan::{self, Paused, Plan, PlanState};

use super::CrewHub;
use crate::hub_core::logins::resolve::Login;
use crate::hub_core::plan_guard::{plan_words, window_words, Held, PlanStanding};
use crate::serve::AgentService;

/// The answer that keeps the limit as it is.
const KEEP: &str = "Keep";
/// The limits offered above the one that holds, lowest first.
const RAISES: [u8; 2] = [95, 100];
/// How many points under the limit a boss is told its plans will pause.
const NEAR: f64 = 10.0;
/// Why the boss leaves the limit to the person, at any level.
const CANT_DECIDE: &str = "Only you can change your plan limit.";

/// What the plan guard last said of a boss's own wake: the plan login it
/// ran on (none: not a plan), and whether the limit held it.
#[derive(Clone)]
pub(super) struct Wake {
    login: Option<Login>,
    held: bool,
}

/// Whether the person's plan limit holds a boss's own wakes now.
enum Limit {
    Holds(Held),
    /// It doesn't: where the plan its last wake ran on stands, if read.
    Lifted(Option<PlanStanding>),
    /// Nothing was noted of its wakes since the hub started.
    Unknown,
}

/// The person's answer to a plan-limit ask.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Answer {
    Raise(u8),
    Keep,
}

fn raise_label(to: u8) -> String {
    format!("Raise to {to}%")
}

/// The lowest limit offered that would let `held` go on.
fn raise_to(held: &Held) -> Option<u8> {
    RAISES
        .into_iter()
        .map(|r| (r, f64::from(r)))
        .find(|(_, r)| *r > held.standing.used_percent && *r > held.at)
        .map(|(r, _)| r)
}

/// What `answer` to the item keyed `key` says, when it asks about a
/// plan-limit pause.
fn answer_of(key: &str, answer: &str) -> Option<Answer> {
    plan::limit_plan(key)?;
    let said = answer.trim().to_lowercase();
    if said == KEEP.to_lowercase() {
        return Some(Answer::Keep);
    }
    said.strip_prefix("raise to ")?
        .strip_suffix('%')?
        .trim()
        .parse::<u8>()
        .ok()
        .filter(|n| (1..=100).contains(n))
        .map(Answer::Raise)
}

/// Whether `answer` to `item` is the person's word on a plan-limit pause,
/// which Desk acts on, never the boss.
pub(super) fn answers(item: &OpenItem, answer: &str) -> bool {
    answer_of(&item.dedupe_key, answer).is_some()
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
    format!("{}, over your {}% limit", held.standing.says(), held.at)
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
    at: Option<f64>,
    until: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    tz: chrono_tz::Tz,
) -> String {
    let why = match (standing, at, until) {
        (Some(s), Some(at), _) if s.used_percent < at => {
            format!("{}, under your {at}% limit", s.says())
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
    at: f64,
    now: DateTime<Utc>,
    tz: chrono_tz::Tz,
) -> Option<String> {
    if let Some(h) = held {
        return Some(format!(
            "Your own turns (check-ins, notices, wake-ups) are paused now: {}, over the person's {}% limit, until {}. Your plans are paused with them. If the person asks you to keep one going, tell them plainly it can't until then unless they raise the limit; only they can.",
            h.standing.says(),
            h.at,
            at_time(h.until, now, tz)
        ));
    }
    let s = standing.filter(|s| s.used_percent >= at - NEAR)?;
    let until = s
        .resets_at
        .map_or_else(String::new, |r| format!(" at {}", at_time(r, now, tz)));
    Some(format!(
        "Your own turns (check-ins, notices, wake-ups) run on your {} {} plan, now at {}%. They pause from {at}%, the person's limit, and your plans pause with them until the window resets{until}. If the person asks you to keep a plan going, or to watch one while they are away, tell them plainly it will pause at {at}% unless they raise the limit; only they can.",
        window_words(s.window_minutes),
        plan_words(s.plan),
        s.used_percent.floor()
    ))
}

impl CrewHub {
    /// Notes what the plan guard said of `service`'s own wake on `login`
    /// at `now`: held, or run on a plan read under the limit (or on none).
    /// A plan not read yet says nothing.
    pub(super) fn note_wake(
        &self,
        service: &AgentService,
        login: Option<Login>,
        now: DateTime<Utc>,
    ) {
        let wake = match login {
            None => Some(Wake {
                login: None,
                held: false,
            }),
            Some(l) => {
                let held = service.cost_governor.plan_holds(&l, now).is_some();
                let read = service
                    .cost_governor
                    .plan_guard()
                    .standing(&l, now)
                    .is_some();
                (held || read).then_some(Wake {
                    login: Some(l),
                    held,
                })
            }
        };
        if let Some(wake) = wake {
            self.wakes
                .lock()
                .insert(service.agent_id.as_str().to_string(), wake);
        }
    }

    fn limit_of(&self, boss: &str, now: DateTime<Utc>) -> Limit {
        let (Some((service, _)), Some(wake)) = (
            self.overseer_named(boss),
            self.wakes.lock().get(boss).cloned(),
        ) else {
            return Limit::Unknown;
        };
        let Some(login) = wake.login else {
            return Limit::Lifted(None);
        };
        match service.cost_governor.plan_holds(&login, now) {
            Some(held) if wake.held => Limit::Holds(held),
            _ => Limit::Lifted(service.cost_governor.plan_guard().standing(&login, now)),
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
            Ok((_, true)) => self.ask_limit(p, &held, now).await,
            Ok(_) => {}
            Err(e) => {
                tracing::info!(plan = %p.id, error = %e, "a plan the plan limit holds wasn't paused")
            }
        }
        true
    }

    /// Asks the person, once for `p` and the window holding it, whether
    /// to raise the limit.
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
    /// boss (or, with nothing noted since a restart, once its window
    /// lapsed), and says so; while it holds, its words are kept current.
    pub(super) async fn lift_limit(&self, p: &Plan) {
        let now = Utc::now();
        let tz = self.desk.timezone();
        let standing = match self.limit_of(&p.boss, now) {
            Limit::Holds(held) => {
                let words = paused_words(&held, now, tz);
                if let Err(e) = self.desk.pause_plan_limit(p.id, &words, held.until).await {
                    tracing::info!(plan = %p.id, error = %e, "a plan's limit pause wasn't kept current");
                }
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
        let at = self
            .overseer_named(&p.boss)
            .and_then(|(s, _)| s.cost_governor.plan_threshold());
        let text = resumed_words(&p.goal, standing.as_ref(), at, p.paused_until, now, tz);
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

    /// The person's answer to `closed`, a plan-limit ask: a raise is kept
    /// and lifts what it lets go at once; Keep changes nothing. What they
    /// are told, or none when `closed` asks nothing of the plan limit.
    pub(super) async fn plan_limit_answered(
        &self,
        closed: &OpenItem,
        answer: &str,
    ) -> Option<String> {
        match answer_of(&closed.dedupe_key, answer)? {
            Answer::Raise(to) => match self.desk.set_plan_pause_at(f64::from(to)).await {
                Ok(_) => {
                    self.lift_limits().await;
                    Some(format!(
                        "Raised: background work on your plans now pauses from {to}%."
                    ))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "the person's plan limit wasn't kept");
                    Some("Desk couldn't change your limit just now. Try again.".into())
                }
            },
            Answer::Keep => {
                let at = self
                    .overseer_named(&closed.boss)
                    .and_then(|(s, _)| s.cost_governor.plan_threshold());
                let plan = match plan::limit_plan(&closed.dedupe_key) {
                    Some(id) => self.desk.store().plan(id).await.ok().flatten(),
                    None => None,
                };
                let kept = at.map_or_else(
                    || "Kept your limit.".to_string(),
                    |at| format!("Kept at {at}%."),
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
                Some(format!("{kept}{after}"))
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
    /// wakes run on for `owner`, when it holds them or is near the limit.
    pub(super) async fn plan_limit_line(
        &self,
        service: &AgentService,
        owner: &str,
    ) -> Option<String> {
        let at = service.cost_governor.plan_threshold()?;
        let noted = self
            .wakes
            .lock()
            .get(service.agent_id.as_str())
            .and_then(|w| w.login.clone());
        let login = match noted {
            Some(l) => l,
            None => {
                let dm = service
                    .sessions
                    .find(workforce_common::channels::DESK_CHANNEL, owner)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| {
                        workforce_common::session::Session::new(
                            workforce_common::channels::DESK_CHANNEL,
                            owner,
                            service.agent_id.as_str(),
                        )
                    });
                let model = crate::turn_loop::heavy_model(service);
                let work = workforce_common::work_use::Work::boss(
                    service.agent_id.as_str(),
                    owner,
                    &super::name_of(service),
                );
                let on = super::deliver::TurnOn {
                    session: &dm,
                    prompt: "",
                    model: model.as_deref(),
                };
                super::deliver::wake_login(service, owner, &work, on).await?
            }
        };
        let now = Utc::now();
        let held = service.cost_governor.plan_holds(&login, now);
        let standing = service.cost_governor.plan_guard().standing(&login, now);
        limit_words(
            held.as_ref(),
            standing.as_ref(),
            at,
            now,
            self.desk.timezone(),
        )
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

    fn held(used: f64, limit: f64) -> Held {
        Held {
            standing: standing(used, "2026-11-27T09:00:00Z"),
            at: limit,
            until: at("2026-11-27T09:00:00Z"),
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
    fn only_a_plan_limit_asks_raise_or_keep_is_read_as_one() {
        let ask = plan::limit_key(uuid::Uuid::new_v4(), at(NOW));
        assert_eq!(answer_of(&ask, "Raise to 95%"), Some(Answer::Raise(95)));
        assert_eq!(answer_of(&ask, " raise to 100% "), Some(Answer::Raise(100)));
        assert_eq!(answer_of(&ask, "keep"), Some(Answer::Keep));
        assert_eq!(answer_of(&ask, "Raise to 0%"), None);
        assert_eq!(answer_of(&ask, "Raise to 120%"), None);
        assert_eq!(answer_of(&ask, "Raise it a bit"), None);
        assert_eq!(answer_of("plan-stopped:a:b", "Keep"), None);
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
