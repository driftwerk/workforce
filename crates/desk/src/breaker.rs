//! The breaker: when a session (or an agent, or a pool of spend) has gone
//! far enough that Desk steps in. Pure: [`step`] and [`pool_step`] fold a
//! signal into where a subject's breaker stands and say what to do; the
//! desk's shell (`service/breaker.rs`) delivers it.
//!
//! It never ends work. A steer is a message into the running turn. A hold
//! lets the turn finish and pauses the subject from its next one (a
//! session's tool calls are refused, an agent's turns are refused, a pool's
//! new starts are refused) until the person says Continue, or types to the
//! session themselves.

use std::collections::VecDeque;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use workforce_core::budget::{OnExceed, Standing, Status};

/// The same tool call this many times...
pub const LOOP_REPEATS: usize = 6;
/// ...within this long, with no turn ending between, is a loop.
pub const LOOP_WINDOW: Duration = Duration::minutes(10);
/// This many failed tool calls...
pub const ERROR_REPEATS: usize = 5;
/// ...or the same failure this many times...
pub const SAME_ERROR_REPEATS: usize = 3;
/// ...within this long.
pub const ERROR_WINDOW: Duration = Duration::minutes(5);
/// A trip this soon after a steer holds.
pub const ESCALATE_WITHIN: Duration = Duration::minutes(15);
/// The same warning isn't repeated within this long.
pub const REPEAT_QUIET: Duration = Duration::minutes(10);
/// How long Continue quiets a loop's or errors' breaker.
pub const RELEASED_FOR: Duration = Duration::minutes(30);

/// What tripped it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "budget", rename_all = "snake_case")]
pub enum Reason {
    Loop,
    Errors,
    Budget(Uuid),
}

/// What a hold is about, to say it in plain words.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Detail {
    Budget {
        standing: Standing,
    },
    Repeats {
        label: String,
        count: usize,
        minutes: i64,
    },
    Failures {
        last: String,
        count: usize,
        minutes: i64,
    },
}

/// Where a subject's breaker stands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(
    tag = "level",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Level {
    #[default]
    Clear,
    /// Told to stop in its turn, at `at`.
    Steered { reason: Reason, at: DateTime<Utc> },
    /// Paused from its next turn. `until`: how long Continue lets it go on
    /// (a budget's period end); `kept`: the person said Keep paused.
    Held {
        reason: Reason,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        detail: Detail,
        #[serde(default)]
        kept: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Seen {
    pub at: DateTime<Utc>,
    pub key: u64,
    pub label: String,
}

/// One subject's breaker, as kept.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub level: Level,
    pub tools: VecDeque<Seen>,
    pub failures: VecDeque<Seen>,
    /// The last warning of each reason (and, for a budget, its status).
    pub warned: Vec<(Reason, Status, DateTime<Utc>)>,
    /// Reasons the person let go on, until when.
    pub released: Vec<(Reason, DateTime<Utc>)>,
    /// Budgets the person let go on at their warning, until when: once
    /// over, they hold again (a pool's or a plan's new starts).
    pub released_warned: Vec<(Reason, DateTime<Utc>)>,
}

impl State {
    pub fn held(&self) -> bool {
        matches!(self.level, Level::Held { .. })
    }

    fn released(&self, reason: &Reason, now: DateTime<Utc>) -> bool {
        self.released
            .iter()
            .any(|(r, until)| r == reason && *until > now)
    }

    /// Whether the person let `reason` go on at its warning, and it is
    /// still only that.
    fn released_warned(&self, reason: &Reason, now: DateTime<Utc>) -> bool {
        self.released_warned
            .iter()
            .any(|(r, until)| r == reason && *until > now)
    }

    fn warned_since(&self, reason: &Reason, status: Status, since: DateTime<Utc>) -> bool {
        self.warned
            .iter()
            .any(|(r, s, at)| r == reason && *s == status && *at >= since)
    }

    fn tidy(&mut self, now: DateTime<Utc>) {
        self.released.retain(|(_, until)| *until > now);
        self.released_warned.retain(|(_, until)| *until > now);
        // A budget's warning counts for its period, at most a month.
        self.warned
            .retain(|(_, _, at)| now - *at < Duration::days(32));
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    /// A tool call started: `key` hashes the call, `label` names it.
    Tool { key: u64, label: String },
    /// A tool call failed.
    Failed { key: u64, label: String },
    /// A turn ended, with any steer already delivered into it.
    Settled,
    /// The subject's standings against the budgets that apply to it.
    Spend(Vec<Standing>),
    /// The person let it go on (or typed to it).
    Continue,
    /// The person said Keep paused.
    Keep,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Deliver into the running turn.
    Steer { reason: Reason, text: String },
    /// Pause it from its next turn, and ask the person.
    Hold { reason: Reason },
    /// The person let it go on.
    Release,
    /// It settled or its period ended: nothing holds it now.
    Clear,
}

fn prune(seen: &mut VecDeque<Seen>, window: Duration, now: DateTime<Utc>) {
    while seen.front().is_some_and(|s| now - s.at > window) {
        seen.pop_front();
    }
}

fn minutes(seen: &VecDeque<Seen>, now: DateTime<Utc>) -> i64 {
    seen.front()
        .map_or(0, |s| (now - s.at).num_minutes().max(1))
}

/// What the session is told when it trips for `reason`.
fn steer_text(detail: &Detail, over: Option<OnExceed>) -> String {
    match (detail, over) {
        (Detail::Failures { last, count, minutes }, _) => format!(
            "Desk's breaker: {count} tool calls have failed in the last {minutes} minutes (the last: {last}). \
             Stop, say what is failing and why, and ask the person how to go on."
        ),
        (Detail::Repeats { label, count, minutes }, _) => format!(
            "Desk's breaker: you've run {label} {count} times in the last {minutes} minutes. \
             Stop, say what you've learned, and ask the person before trying it again."
        ),
        (Detail::Budget { standing }, None) => format!(
            "Desk's breaker: you have used {} of your {} budget ({}). Keep to what's needed and \
             wrap up soon.",
            standing.percent(),
            standing.period.words(),
            standing.amounts()
        ),
        (Detail::Budget { standing }, Some(OnExceed::Pause)) => format!(
            "Desk's breaker: you are over your {} budget ({}). Finish the step you're on and end \
             your turn: Desk pauses you after it until the person says to go on.",
            standing.period.words(),
            standing.amounts()
        ),
        (Detail::Budget { standing }, Some(OnExceed::Warn)) => format!(
            "Desk's breaker: you are over your {} budget ({}). The person asked only to be told; \
             keep to what's needed.",
            standing.period.words(),
            standing.amounts()
        ),
    }
}

/// Trips for a loop or errors: a steer first; a second trip soon after a
/// steer holds from the next turn.
fn trip(mut s: State, reason: Reason, detail: Detail, now: DateTime<Utc>) -> (State, Vec<Step>) {
    if s.released(&reason, now) || s.held() {
        return (s, Vec::new());
    }
    match &s.level {
        Level::Steered { at, .. } if now - *at <= ESCALATE_WITHIN => {
            s.level = Level::Held {
                reason: reason.clone(),
                since: now,
                until: now + RELEASED_FOR,
                detail,
                kept: false,
            };
            (s, vec![Step::Hold { reason }])
        }
        _ if s.warned_since(&reason, Status::Over, now - REPEAT_QUIET) => (s, Vec::new()),
        _ => {
            let text = steer_text(&detail, None);
            s.warned.push((reason.clone(), Status::Over, now));
            s.level = Level::Steered {
                reason: reason.clone(),
                at: now,
            };
            (s, vec![Step::Steer { reason, text }])
        }
    }
}

/// Folds `signal` into a session's or agent's breaker at `now`.
pub fn step(mut s: State, signal: Signal, now: DateTime<Utc>) -> (State, Vec<Step>) {
    s.tidy(now);
    match signal {
        Signal::Tool { key, label } => {
            prune(&mut s.tools, LOOP_WINDOW, now);
            s.tools.push_back(Seen {
                at: now,
                key,
                label: label.clone(),
            });
            let count = s.tools.iter().filter(|t| t.key == key).count();
            if count < LOOP_REPEATS {
                return (s, Vec::new());
            }
            let detail = Detail::Repeats {
                label,
                count,
                minutes: minutes(&s.tools, now),
            };
            s.tools.retain(|t| t.key != key);
            trip(s, Reason::Loop, detail, now)
        }
        Signal::Failed { key, label } => {
            prune(&mut s.failures, ERROR_WINDOW, now);
            s.failures.push_back(Seen {
                at: now,
                key,
                label: label.clone(),
            });
            let same = s.failures.iter().filter(|f| f.key == key).count();
            let all = s.failures.len();
            if same < SAME_ERROR_REPEATS && all < ERROR_REPEATS {
                return (s, Vec::new());
            }
            let detail = Detail::Failures {
                last: label,
                count: all,
                minutes: minutes(&s.failures, now),
            };
            s.failures.clear();
            trip(s, Reason::Errors, detail, now)
        }
        Signal::Settled => {
            s.tools.clear();
            s.failures.clear();
            match s.level {
                Level::Steered { .. } => {
                    s.level = Level::Clear;
                    (s, vec![Step::Clear])
                }
                Level::Clear | Level::Held { .. } => (s, Vec::new()),
            }
        }
        Signal::Continue => match std::mem::take(&mut s.level) {
            Level::Held {
                reason,
                until,
                detail,
                ..
            } => {
                let until = match reason {
                    Reason::Budget(_) => until,
                    Reason::Loop | Reason::Errors => now + RELEASED_FOR,
                };
                // Let go on at its warning, it holds again once over.
                let warned = matches!(&detail, Detail::Budget { standing } if standing.status == Status::Warn);
                match warned {
                    true => s.released_warned.push((reason, until)),
                    false => s.released.push((reason, until)),
                }
                (s, vec![Step::Release])
            }
            other => {
                s.level = other;
                (s, Vec::new())
            }
        },
        Signal::Keep => {
            if let Level::Held { kept, .. } = &mut s.level {
                *kept = true;
            }
            (s, Vec::new())
        }
        Signal::Spend(standings) => spend(s, standings, now),
    }
}

/// A subject's standings, the tightest first.
fn ordered(mut standings: Vec<Standing>) -> Vec<Standing> {
    standings.sort_by(|a, b| {
        b.status
            .cmp(&a.status)
            .then(b.share().total_cmp(&a.share()))
    });
    standings
}

/// Whether a budget hold's period is over and its budget no longer over.
fn hold_lapsed(level: &Level, standings: &[Standing], now: DateTime<Utc>) -> bool {
    match level {
        Level::Held {
            reason: Reason::Budget(id),
            until,
            ..
        } => {
            now >= *until
                && !standings
                    .iter()
                    .any(|st| st.budget == *id && st.status == Status::Over)
        }
        _ => false,
    }
}

fn spend(mut s: State, standings: Vec<Standing>, now: DateTime<Utc>) -> (State, Vec<Step>) {
    let mut steps = Vec::new();
    if hold_lapsed(&s.level, &standings, now) {
        s.level = Level::Clear;
        steps.push(Step::Clear);
    }
    for st in ordered(standings) {
        let reason = Reason::Budget(st.budget);
        let detail = Detail::Budget {
            standing: st.clone(),
        };
        match (st.status, st.on_exceed) {
            (Status::Under, _) => {}
            (Status::Warn, _) => {
                if !s.warned_since(&reason, Status::Warn, st.from) && !s.held() {
                    s.warned.push((reason.clone(), Status::Warn, now));
                    steps.push(Step::Steer {
                        text: steer_text(&detail, None),
                        reason,
                    });
                }
            }
            (Status::Over, OnExceed::Warn) => {
                if !s.warned_since(&reason, Status::Over, st.from) {
                    s.warned.push((reason.clone(), Status::Over, now));
                    steps.push(Step::Steer {
                        text: steer_text(&detail, Some(OnExceed::Warn)),
                        reason,
                    });
                }
            }
            (Status::Over, OnExceed::Pause) => {
                if s.held() || s.released(&reason, now) {
                    continue;
                }
                if !s.warned_since(&reason, Status::Over, st.from) {
                    s.warned.push((reason.clone(), Status::Over, now));
                    steps.push(Step::Steer {
                        text: steer_text(&detail, Some(OnExceed::Pause)),
                        reason: reason.clone(),
                    });
                }
                s.level = Level::Held {
                    reason: reason.clone(),
                    since: now,
                    until: st.to,
                    detail,
                    kept: false,
                };
                steps.push(Step::Hold { reason });
            }
        }
    }
    (s, steps)
}

/// Folds a pool's standings (Desk's, or a group's), or a plan's, into its
/// breaker: at its warning, or over, new starts are held for the rest of
/// the period, unless the budget only warns or the person let them go on.
/// Let go on at the warning, they are held again once it is over.
pub fn pool_step(mut s: State, signal: Signal, now: DateTime<Utc>) -> (State, Vec<Step>) {
    s.tidy(now);
    match signal {
        Signal::Spend(standings) => {
            let mut steps = Vec::new();
            if hold_lapsed(&s.level, &standings, now) {
                s.level = Level::Clear;
                steps.push(Step::Clear);
            }
            if s.held() {
                return (s, steps);
            }
            let tripped = ordered(standings).into_iter().find(|st| {
                let reason = Reason::Budget(st.budget);
                st.status >= Status::Warn
                    && st.on_exceed == OnExceed::Pause
                    && !s.released(&reason, now)
                    && !(st.status == Status::Warn && s.released_warned(&reason, now))
            });
            if let Some(st) = tripped {
                let reason = Reason::Budget(st.budget);
                s.level = Level::Held {
                    reason: reason.clone(),
                    since: now,
                    until: st.to,
                    detail: Detail::Budget { standing: st },
                    kept: false,
                };
                steps.push(Step::Hold { reason });
            }
            (s, steps)
        }
        Signal::Continue | Signal::Keep => step(s, signal, now),
        Signal::Tool { .. } | Signal::Failed { .. } | Signal::Settled => (s, Vec::new()),
    }
}

/// What a held subject is, for the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whose {
    Session,
    Agent,
    Pool,
    /// A plan's new starts.
    Plan,
}

/// What the Floor says of a hold: the ask, and what each answer does.
pub fn need_text(name: &str, whose: Whose, level: &Level) -> Option<(String, String)> {
    need_text_for(name, whose, level, None)
}

/// [`need_text`], for a pool or a plan whose new sessions `starter`, its
/// boss, starts: it is the one that has stopped.
pub fn need_text_for(
    name: &str,
    whose: Whose,
    level: &Level,
    starter: Option<&str>,
) -> Option<(String, String)> {
    let Level::Held { detail, .. } = level else {
        return None;
    };
    let stopped = starter.map(|b| format!("{b} has stopped starting sessions."));
    let paused = match (whose, stopped.as_deref()) {
        (Whose::Session, _) => "Desk paused it after its turn.",
        (Whose::Agent, _) => "Desk paused its new turns; the one it was on finished.",
        (Whose::Pool | Whose::Plan, Some(b)) => b,
        (Whose::Pool, None) => "Agents can't start new sessions for it.",
        (Whose::Plan, None) => "Desk has stopped starting its sessions.",
    };
    Some(match detail {
        Detail::Budget { standing } => {
            let rest = match standing.period {
                workforce_core::budget::Period::Total => "of its run".to_string(),
                p => format!("of the {}", p.words()),
            };
            let until = match standing.status {
                Status::Warn => ", until it is over",
                Status::Under | Status::Over => "",
            };
            let why = match whose {
                Whose::Plan => format!(
                    "Its running sessions carry on under their own limits. Continue lets new \
                     starts go on for the rest {rest}{until}; Keep paused leaves them held."
                ),
                Whose::Pool => format!(
                    "Running sessions carry on under their own limits. Continue lets new starts go \
                     on for the rest {rest}{until}; Keep paused leaves them held."
                ),
                Whose::Session | Whose::Agent => format!(
                    "Continue lets it go on for the rest {rest}; Keep paused leaves it paused \
                     until you say."
                ),
            };
            (
                format!(
                    "{name} is at {} of its {} budget ({}). {paused}",
                    standing.percent(),
                    standing.period.words(),
                    standing.amounts()
                ),
                why,
            )
        }
        Detail::Repeats { .. } | Detail::Failures { .. } => {
            let what = match detail {
                Detail::Failures {
                    last,
                    count,
                    minutes,
                } => format!(
                    "{name} kept failing ({count} failed tool calls in {minutes} minutes, the last \
                     {last}) after Desk's breaker told it to stop."
                ),
                Detail::Repeats {
                    label,
                    count,
                    minutes,
                } => format!(
                    "{name} kept running {label} ({count} times in {minutes} minutes) after Desk's \
                     breaker told it to stop."
                ),
                Detail::Budget { .. } => unreachable!("matched above"),
            };
            (
                format!("{what} {paused}"),
                "Continue lets it go on, and the breaker stays quiet on this for 30 minutes; Keep \
                 paused leaves it paused until you say."
                    .to_string(),
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_core::budget::{Period, Unit};

    fn at(m: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-03T10:00:00Z")
            .unwrap()
            .to_utc()
            + Duration::minutes(m)
    }

    fn tool(key: u64) -> Signal {
        Signal::Tool {
            key,
            label: "Bash(cargo test -p atlas)".into(),
        }
    }

    fn run(mut s: State, signals: impl IntoIterator<Item = (i64, Signal)>) -> (State, Vec<Step>) {
        let mut all = Vec::new();
        for (m, sig) in signals {
            let (next, steps) = step(s, sig, at(m));
            s = next;
            all.extend(steps);
        }
        (s, all)
    }

    fn standing(id: Uuid, used: f64, status: Status, on_exceed: OnExceed) -> Standing {
        Standing {
            budget: id,
            period: Period::Day,
            unit: Unit::Usd,
            used,
            of: 11.0,
            status,
            on_exceed,
            from: at(-600),
            to: at(840),
        }
    }

    #[test]
    fn the_same_call_six_times_steers_and_once_more_after_the_steer_holds() {
        let (s, steps) = run(State::default(), (0..6).map(|m| (m, tool(7))));
        assert!(
            matches!(&steps[..], [Step::Steer { reason: Reason::Loop, text }]
            if text.contains("cargo test -p atlas") && text.contains("6 times"))
        );
        let (s, steps) = run(s, (6..12).map(|m| (m, tool(7))));
        assert_eq!(
            steps,
            [Step::Hold {
                reason: Reason::Loop
            }]
        );
        assert!(s.held());
        let (ask, why) = need_text("atlas-api", Whose::Session, &s.level).unwrap();
        assert!(
            ask.contains("kept running") && ask.contains("after its turn"),
            "{ask}"
        );
        assert!(why.contains("Continue"));
    }

    #[test]
    fn a_session_that_settles_after_its_steer_starts_again_at_a_steer() {
        let (s, _) = run(State::default(), (0..6).map(|m| (m, tool(7))));
        let (s, steps) = run(s, [(7, Signal::Settled)]);
        assert_eq!(steps, [Step::Clear]);
        assert_eq!(s.level, Level::Clear);
        // Within the quiet time nothing is said again; after it, a steer.
        let (s, steps) = run(s, (8..14).map(|m| (m, tool(7))));
        assert!(steps.is_empty() && !s.held());
        let (_, steps) = run(s, (30..36).map(|m| (m, tool(7))));
        assert!(matches!(&steps[..], [Step::Steer { .. }]));
    }

    #[test]
    fn calls_spread_beyond_the_window_or_split_by_a_turn_end_are_no_loop() {
        let (_, steps) = run(State::default(), (0..6).map(|m| (m * 3, tool(7))));
        assert!(steps.is_empty());
        let mut signals: Vec<(i64, Signal)> = (0..3).map(|m| (m, tool(7))).collect();
        signals.push((3, Signal::Settled));
        signals.extend((4..7).map(|m| (m, tool(7))));
        let (_, steps) = run(State::default(), signals);
        assert!(steps.is_empty());
    }

    #[test]
    fn the_same_failure_three_times_steers_with_what_failed() {
        let fail = |key| Signal::Failed {
            key,
            label: "Bash: error[E0425]".into(),
        };
        let (_, steps) = run(State::default(), (0..3).map(|m| (m, fail(1))));
        assert!(
            matches!(&steps[..], [Step::Steer { reason: Reason::Errors, text }]
            if text.contains("have failed") && text.contains("E0425"))
        );
        let (_, steps) = run(State::default(), (0..4).map(|m| (m, fail(m as u64))));
        assert!(steps.is_empty(), "four different failures");
        let (_, steps) = run(State::default(), (0..5).map(|m| (m, fail(m as u64))));
        assert_eq!(steps.len(), 1, "five failures");
    }

    #[test]
    fn continue_releases_a_loop_for_half_an_hour_and_keep_paused_stays_held() {
        let (s, _) = run(State::default(), (0..12).map(|m| (m, tool(7))));
        let (s, steps) = run(s, [(13, Signal::Keep)]);
        assert!(steps.is_empty());
        assert!(matches!(s.level, Level::Held { kept: true, .. }));
        let (s, steps) = run(s, [(14, Signal::Continue)]);
        assert_eq!(steps, [Step::Release]);
        assert_eq!(s.level, Level::Clear);
        let (s, steps) = run(s, (15..27).map(|m| (m, tool(7))));
        assert!(steps.is_empty(), "released for 30 minutes");
        let (s, _) = run(s, [(50, Signal::Settled)]);
        let (_, steps) = run(s, (50..56).map(|m| (m, tool(7))));
        assert!(
            matches!(&steps[..], [Step::Steer { .. }]),
            "and then it watches again"
        );
    }

    #[test]
    fn a_budget_warns_once_a_period_and_over_it_holds_until_the_period_ends() {
        let id = Uuid::new_v4();
        let warn = Signal::Spend(vec![standing(id, 9.1, Status::Warn, OnExceed::Pause)]);
        let (s, steps) = run(State::default(), [(0, warn.clone())]);
        assert!(matches!(&steps[..], [Step::Steer { text, .. }] if text.contains("82%")));
        let (s, steps) = run(s, [(5, warn)]);
        assert!(steps.is_empty(), "once a period");
        let over = Signal::Spend(vec![standing(id, 11.2, Status::Over, OnExceed::Pause)]);
        let (s, steps) = run(s, [(10, over.clone())]);
        assert!(
            matches!(&steps[..], [Step::Steer { text, .. }, Step::Hold { .. }]
            if text.contains("end your turn"))
        );
        let (ask, _) = need_text("atlas-api", Whose::Session, &s.level).unwrap();
        assert_eq!(ask, "atlas-api is at 101% of its day budget ($11.20 of $11). Desk paused it after its turn.");
        let (s, steps) = run(s, [(11, over.clone())]);
        assert!(steps.is_empty(), "held once");
        let (s, steps) = run(s, [(12, Signal::Continue)]);
        assert_eq!(steps, [Step::Release]);
        let (s, steps) = run(s, [(13, over.clone())]);
        assert!(steps.is_empty(), "released for the rest of the day");
        // The next day it starts again from nothing.
        let next_day = Signal::Spend(vec![Standing {
            from: at(840),
            to: at(2280),
            ..standing(id, 11.5, Status::Over, OnExceed::Pause)
        }]);
        let (_, steps) = run(s, [(841, next_day)]);
        assert!(steps.contains(&Step::Hold {
            reason: Reason::Budget(id)
        }));
    }

    #[test]
    fn a_held_budget_lapses_when_its_period_ends_under_it() {
        let id = Uuid::new_v4();
        let over = Signal::Spend(vec![standing(id, 12.0, Status::Over, OnExceed::Pause)]);
        let (s, _) = run(State::default(), [(0, over)]);
        let under = Signal::Spend(vec![standing(id, 0.0, Status::Under, OnExceed::Pause)]);
        let (s, steps) = run(s, [(100, under.clone())]);
        assert!(steps.is_empty() && s.held(), "the day isn't over");
        let (s, steps) = run(s, [(841, under)]);
        assert_eq!(steps, [Step::Clear]);
        assert!(!s.held());
    }

    #[test]
    fn a_budget_that_only_warns_never_holds() {
        let id = Uuid::new_v4();
        let over = Signal::Spend(vec![standing(id, 12.0, Status::Over, OnExceed::Warn)]);
        let (s, steps) = run(State::default(), [(0, over.clone())]);
        assert!(
            matches!(&steps[..], [Step::Steer { text, .. }] if text.contains("only to be told"))
        );
        let (s, steps) = run(s, [(1, over)]);
        assert!(steps.is_empty() && !s.held());
    }

    #[test]
    fn a_pool_holds_new_starts_from_its_warning_and_continue_holds_off_for_the_period() {
        let id = Uuid::new_v4();
        let warn = Signal::Spend(vec![standing(id, 9.13, Status::Warn, OnExceed::Pause)]);
        let (s, steps) = pool_step(State::default(), warn.clone(), at(0));
        assert_eq!(
            steps,
            [Step::Hold {
                reason: Reason::Budget(id)
            }]
        );
        let (ask, why) = need_text("Atlas", Whose::Pool, &s.level).unwrap();
        assert_eq!(ask, "Atlas is at 83% of its day budget ($9.13 of $11). Agents can't start new sessions for it.");
        assert!(why.starts_with("Running sessions carry on under their own limits."));
        assert!(why.contains("until it is over"), "{why}");
        let (ask, _) =
            need_text_for("The plan “Ship it”", Whose::Plan, &s.level, Some("Sarah")).unwrap();
        assert_eq!(ask, "The plan “Ship it” is at 83% of its day budget ($9.13 of $11). Sarah has stopped starting sessions.");
        let (s, steps) = pool_step(s, Signal::Continue, at(1));
        assert_eq!(steps, [Step::Release]);
        let (s, steps) = pool_step(s, warn, at(2));
        assert!(steps.is_empty() && !s.held());
        // Let go on at its warning, it holds again once over.
        let over = Signal::Spend(vec![standing(id, 11.2, Status::Over, OnExceed::Pause)]);
        let (s, steps) = pool_step(s, over.clone(), at(3));
        assert_eq!(
            steps,
            [Step::Hold {
                reason: Reason::Budget(id)
            }]
        );
        let (_, why) = need_text("Atlas", Whose::Pool, &s.level).unwrap();
        assert!(!why.contains("until it is over"), "{why}");
        let (s, _) = pool_step(s, Signal::Continue, at(4));
        let (s, steps) = pool_step(s, over, at(5));
        assert!(steps.is_empty() && !s.held(), "over, let go for the day");
        let back: State = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let (_, steps) = pool_step(
            State::default(),
            Signal::Spend(vec![standing(id, 9.13, Status::Warn, OnExceed::Warn)]),
            at(0),
        );
        assert!(steps.is_empty());
    }

    #[test]
    fn a_kept_state_reads_back_as_it_was() {
        let (s, _) = run(State::default(), (0..12).map(|m| (m, tool(7))));
        let back: State = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let empty: State = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(empty, State::default());
    }
}
