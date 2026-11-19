//! The plan guard: the budget of background work on a subscription.
//!
//! A dollar ceiling means nothing on a person's Claude or ChatGPT plan, but
//! the plan's own windows are real. Here is what the hub knows of them and
//! what it does with that:
//!
//! - **What is read.** Each call on a plan reports its windows: Claude Code's
//!   `rate_limit_event` (`unifiedWindows`, the windows the plan has, by
//!   name; only `five_hour` and `seven_day` have a known length, so a model's
//!   own weekly window is not read), ChatGPT's rate-limit headers (`primary`
//!   and `secondary`, each with its length), and, on the person's own hub,
//!   what this Mac's Claude Code and Codex report. Only windows a plan
//!   reports are read: a plan with no five-hour window has none here.
//! - **Pause.** Background work ([`workforce_common::work_use`]) waits while
//!   any live window of the plan its next turn would run on is at
//!   `[budget.plan] pause_at_percent` or more. It goes on once a newer
//!   reading is under, or the window's reset has passed: background work
//!   that waits makes no calls, so the reset is what frees it when nothing
//!   else reads the plan. With no reading at all, it runs.
//! - **Runaway.** A unit of background work using far more than its usual
//!   share of the plan, or far more turns, in the last hour is stopped
//!   ([`Admission::Stopped`](super::cost_governor::Admission::Stopped)), and
//!   its owner told once, by what stops it (a job is turned off).
//! - **Share.** What each call moved a plan's windows by is counted toward
//!   the work it was made for: the move since the account's last reading,
//!   from any source, so the person's own use between two of a job's calls
//!   is not the job's. Figures are "about": two calls at once share a move.
//!
//! The person's own turns are never held here. Readings are kept in memory
//! per hub instance: after a restart, or on an instance that has served no
//! call on a plan yet, there is no reading, and nothing is held until there
//! is one.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use workforce_common::settings::PlanGuardConfig;
use workforce_common::subscription::SubscriptionKind;
use workforce_common::telemetry::TelemetryStore;
use workforce_common::usage::{account_key, PlanKind, Window};
#[cfg(test)]
use workforce_common::work_use::Allowed;
use workforce_common::work_use::{self, Work, WorkUse, WorkUseRow};

use super::logins::resolve::{Credential, Login};

/// How long a unit of work's last hour and week are trusted before they
/// are read again.
const STATS_TTL: Duration = Duration::from_secs(30);

/// How long a runaway that nothing else turns off stays stopped.
const RUNAWAY_HOLD: chrono::Duration = chrono::Duration::hours(1);

/// The usual is read over this many hours before the last one.
const USUAL_HOURS: i64 = 7 * 24;

/// A plan's windows as last read.
#[derive(Debug, Clone, PartialEq)]
struct Book {
    plan: PlanKind,
    windows: Vec<Window>,
    at: DateTime<Utc>,
}

/// Where a plan stands: its fullest live window.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanStanding {
    pub plan: PlanKind,
    pub used_percent: f64,
    pub window_minutes: u32,
    pub resets_at: Option<DateTime<Utc>>,
}

impl PlanStanding {
    /// "your weekly Claude plan is at 83%".
    pub fn says(&self) -> String {
        format!(
            "your {} {} plan is at {}%",
            window_words(self.window_minutes),
            plan_words(self.plan),
            self.used_percent.floor() as i64
        )
    }
}

/// "weekly", "five-hour", "3-hour", "2-day".
pub fn window_words(minutes: u32) -> String {
    match minutes {
        10_080 => "weekly".into(),
        300 => "five-hour".into(),
        m if m % 1_440 == 0 => format!("{}-day", m / 1_440),
        m if m % 60 == 0 => format!("{}-hour", m / 60),
        m => format!("{m}-minute"),
    }
}

/// "Claude", "ChatGPT".
pub fn plan_words(plan: PlanKind) -> &'static str {
    match plan {
        PlanKind::Claude => "Claude",
        PlanKind::Chatgpt => "ChatGPT",
    }
}

/// The plan a subscription login draws on.
pub fn plan_of(kind: SubscriptionKind) -> PlanKind {
    match kind {
        SubscriptionKind::Claude => PlanKind::Claude,
        SubscriptionKind::ChatGpt => PlanKind::Chatgpt,
    }
}

/// Whether `w` still counts at `now`: its reset is ahead, or (with no
/// reset said) its length since it was read is.
fn live(w: &Window, read_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    match w.resets_at {
        Some(reset) => reset > now,
        None => read_at + chrono::Duration::minutes(i64::from(w.minutes)) > now,
    }
}

/// What background work did, and why the guard says what it does.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Proceed,
    /// Waits: its plan is past the threshold.
    Waits(String),
    /// Stopped as a runaway. `first`: this check stopped it (tell the
    /// person); else it was stopped before.
    Stopped {
        why: String,
        first: bool,
    },
}

/// One unit of work's use in the last hour against its usual.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rate {
    pub hour_share: f64,
    pub usual_share: f64,
    pub hour_turns: u64,
    pub usual_turns: f64,
    /// The plan and window its share is of.
    pub of: Option<(PlanKind, u32)>,
}

/// A work's rate as last read: when, and from when it counted.
type CachedRate = (Instant, Option<DateTime<Utc>>, Rate);

/// A runaway stopped: when, until when, and why.
#[derive(Debug, Clone)]
struct Stop {
    at: DateTime<Utc>,
    until: DateTime<Utc>,
    why: String,
}

/// The guard's state: what is known of each plan, what each call moved and
/// is not yet counted, and the runaways stopped.
pub struct PlanGuard {
    books: Mutex<HashMap<String, Book>>,
    unclaimed: Mutex<HashMap<String, Vec<(PlanKind, u32, f64)>>>,
    stops: Mutex<HashMap<String, Stop>>,
    released: Mutex<HashMap<String, DateTime<Utc>>>,
    rates: Mutex<HashMap<String, CachedRate>>,
    store: parking_lot::RwLock<Option<Arc<dyn TelemetryStore>>>,
}

static GLOBAL: LazyLock<Arc<PlanGuard>> = LazyLock::new(|| Arc::new(PlanGuard::new()));

impl Default for PlanGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl PlanGuard {
    pub fn new() -> Self {
        Self {
            books: Mutex::new(HashMap::new()),
            unclaimed: Mutex::new(HashMap::new()),
            stops: Mutex::new(HashMap::new()),
            released: Mutex::new(HashMap::new()),
            rates: Mutex::new(HashMap::new()),
            store: parking_lot::RwLock::new(None),
        }
    }

    /// The hub's.
    pub fn global() -> Arc<Self> {
        Arc::clone(&GLOBAL)
    }

    /// Keep what background work uses in `store` from now on.
    /// Keep what background work uses in `store`: the first store given
    /// is kept (a hub has one; a test process composes many).
    pub fn keep_in(&self, store: Arc<dyn TelemetryStore>) {
        let mut kept = self.store.write();
        if kept.is_none() {
            *kept = Some(store);
        }
    }

    /// The store what background work uses is kept in.
    pub fn store(&self) -> Option<Arc<dyn TelemetryStore>> {
        self.store.read().clone()
    }

    /// The keys `login`'s readings are kept under: the login, and the plan's
    /// account where that can be told.
    fn keys_of(login: &Login) -> Vec<String> {
        let plan = plan_of(login.kind);
        let mut keys = vec![login_key(login)];
        let account = match &login.credential {
            Credential::ChatGpt(auth) => auth.account_id().and_then(|id| account_key(plan, &[id])),
            Credential::Pooled(id) => super::logins::install_pool::of(&login.slot.vault)
                .and_then(|pool| pool.plan_key(id)),
            Credential::Token(_) => None,
        };
        keys.extend(account.map(|a| account_book(&a)));
        keys
    }

    /// The freshest reading under any of `keys`.
    fn freshest(&self, keys: &[String]) -> Option<Book> {
        let books = self.books.lock();
        keys.iter()
            .filter_map(|k| books.get(k))
            .max_by_key(|b| b.at)
            .cloned()
    }

    /// A call on `login` read its plan's `windows` at `at`: kept, and what
    /// moved since the plan was last read is the call's, to be counted when
    /// it is served.
    pub fn observe(&self, login: &Login, windows: &[Window], at: DateTime<Utc>) {
        if windows.is_empty() {
            return;
        }
        let keys = Self::keys_of(login);
        let plan = plan_of(login.kind);
        let moved = self
            .freshest(&keys)
            .filter(|before| before.at <= at)
            .map(|before| moved(&before, windows))
            .unwrap_or_default();
        if !moved.is_empty() {
            let mut unclaimed = self.unclaimed.lock();
            let mine = unclaimed.entry(login_key(login)).or_default();
            for (minutes, share) in moved {
                match mine
                    .iter_mut()
                    .find(|(p, m, _)| *p == plan && *m == minutes)
                {
                    Some(entry) => entry.2 += share,
                    None => mine.push((plan, minutes, share)),
                }
            }
        }
        self.keep(&keys, plan, windows, at);
    }

    /// This computer's own CLI read `plan`'s windows for the account known
    /// by `account` (as [`account_key`] gives it): kept, its use no work's.
    pub fn observe_account(
        &self,
        plan: PlanKind,
        account: &str,
        windows: &[Window],
        at: DateTime<Utc>,
    ) {
        if windows.is_empty() {
            return;
        }
        self.keep(&[account_book(account)], plan, windows, at);
    }

    fn keep(&self, keys: &[String], plan: PlanKind, windows: &[Window], at: DateTime<Utc>) {
        let mut books = self.books.lock();
        for key in keys {
            if books.get(key).is_some_and(|b| b.at > at) {
                continue;
            }
            books.insert(
                key.clone(),
                Book {
                    plan,
                    windows: windows.to_vec(),
                    at,
                },
            );
        }
    }

    /// A call on `login` was served: what it moved is counted toward the
    /// background work it was for, if any.
    pub fn served(&self, login: &Login, payer: &str) {
        let moved = self.unclaimed.lock().remove(&login_key(login));
        let (Some(moved), Some(work), Some(store)) = (moved, work_use::current(), self.store())
        else {
            return;
        };
        let now = Utc::now();
        for (plan, minutes, share) in moved {
            let row = WorkUseRow::share(&work, Some(payer), plan.as_str(), minutes, share, now);
            if let Err(e) = store.record_work_use(&row) {
                tracing::warn!(error = %e, work = %work.key, "Failed to keep a background work's plan share");
            }
        }
    }

    /// Where `login`'s plan stands at `now`: its fullest live window, if it
    /// has been read.
    pub fn standing(&self, login: &Login, now: DateTime<Utc>) -> Option<PlanStanding> {
        let book = self.freshest(&Self::keys_of(login))?;
        book.windows
            .iter()
            .filter(|w| live(w, book.at, now))
            .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
            .map(|w| PlanStanding {
                plan: book.plan,
                used_percent: w.used_percent,
                window_minutes: w.minutes,
                resets_at: w.resets_at,
            })
    }

    /// Whether background `work` runs its next turn, which would run on
    /// `login` (`None`: not on a plan, or none could be told). A runaway is
    /// judged by what the person said of it ([`Work::allowed`]).
    pub fn check(
        &self,
        cfg: &PlanGuardConfig,
        work: &Work,
        login: Option<&Login>,
        now: DateTime<Utc>,
    ) -> Verdict {
        let allowed = work.allowed;
        if !cfg.enabled {
            return Verdict::Proceed;
        }
        if let Some(why) = self.stopped(&work.key, allowed.resumed_at, now) {
            return Verdict::Stopped { why, first: false };
        }
        if let Some(standing) = login.and_then(|l| self.standing(l, now)) {
            if standing.used_percent >= cfg.pause_at() {
                return Verdict::Waits(waits_why(&standing, cfg.pause_at()));
            }
        }
        let since = [
            allowed.resumed_at,
            self.released.lock().get(&work.key).copied(),
        ]
        .into_iter()
        .flatten()
        .max();
        let rate = self.rate(work, since, now);
        if let Some(why) = runaway(cfg, work, &rate, allowed.hourly_limit) {
            self.stops.lock().insert(
                work.key.clone(),
                Stop {
                    at: now,
                    until: now + RUNAWAY_HOLD,
                    why: why.clone(),
                },
            );
            return Verdict::Stopped { why, first: true };
        }
        Verdict::Proceed
    }

    /// Why `work` is stopped as a runaway now, while it is: until its hold
    /// lapses, or the person resumes it after it was stopped.
    fn stopped(
        &self,
        key: &str,
        resumed_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Option<String> {
        let mut stops = self.stops.lock();
        match stops.get(key) {
            Some(stop) if stop.until > now && resumed_at.is_none_or(|r| r < stop.at) => {
                Some(stop.why.clone())
            }
            Some(_) => {
                stops.remove(key);
                None
            }
            None => None,
        }
    }

    /// Lets `work` go on: the person resumed it. What it did before now is
    /// not held against it.
    pub fn release(&self, key: &str) {
        self.stops.lock().remove(key);
        self.rates.lock().remove(key);
        self.released.lock().insert(key.to_string(), Utc::now());
    }

    /// A turn of `work` began, billed to `payer`.
    pub fn began(&self, work: &Work, payer: Option<&str>) {
        let Some(store) = self.store() else {
            return;
        };
        if let Err(e) = store.record_work_use(&WorkUseRow::turn(work, payer, Utc::now())) {
            tracing::warn!(error = %e, work = %work.key, "Failed to keep a background work's turn");
        }
        // The next check reads it: a loop is caught on the turn that makes it one.
        self.rates.lock().remove(&work.key);
    }

    /// `work`'s last hour (from `since`, when later) against its usual,
    /// cached for [`STATS_TTL`].
    fn rate(&self, work: &Work, since: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Rate {
        if let Some((at, of, rate)) = self.rates.lock().get(&work.key) {
            if at.elapsed() < STATS_TTL && *of == since {
                return rate.clone();
            }
        }
        let Some(store) = self.store() else {
            return Rate::default();
        };
        let from = now - chrono::Duration::hours(USUAL_HOURS + 1);
        let rows = match store.work_use_rows(from, now, None, Some(&work.key)) {
            Ok(rows) => rows,
            Err(e) => {
                // Fails open: a broken store never stops work.
                tracing::warn!(error = %e, work = %work.key, "Reading a background work's use failed");
                return Rate::default();
            }
        };
        let rate = rate_of(&rows, since, now);
        self.rates
            .lock()
            .insert(work.key.clone(), (Instant::now(), since, rate.clone()));
        rate
    }
}

/// The key a login's readings are kept under.
fn login_key(login: &Login) -> String {
    format!("login:{}:{}", login.kind.label(), login.key.fingerprint)
}

/// The key an account's readings are kept under.
fn account_book(account: &str) -> String {
    format!("account:{account}")
}

/// What `windows` moved since `before`, per window length: a window that
/// reset since counts from nothing.
fn moved(before: &Book, windows: &[Window]) -> Vec<(u32, f64)> {
    windows
        .iter()
        .filter_map(|w| {
            let prior = before.windows.iter().find(|b| b.minutes == w.minutes)?;
            let same = match (prior.resets_at, w.resets_at) {
                (Some(a), Some(b)) => (a - b).num_seconds().abs() < 120,
                _ => true,
            };
            let delta = if same {
                w.used_percent - prior.used_percent
            } else {
                w.used_percent
            };
            (delta > 0.0).then_some((w.minutes, delta))
        })
        .collect()
}

fn waits_why(standing: &PlanStanding, at: f64) -> String {
    let until = standing.resets_at.map_or_else(String::new, |r| {
        format!(", or until it resets at {}", r.format("%a %H:%M UTC"))
    });
    format!(
        "Background work is waiting: {}, and background work pauses from {}% so the plan is left for you. It goes on by itself once usage is under that{until}.",
        standing.says(),
        at.floor() as i64
    )
}

/// `rows` of one unit of work over the last hour and the week before it.
/// What it did in the hour before `since` (the person resumed it then) is
/// neither the hour's nor its usual.
pub fn rate_of(rows: &[WorkUseRow], since: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Rate {
    let hour_ago = now - chrono::Duration::hours(1);
    let (hour, before): (Vec<_>, Vec<_>) = rows
        .iter()
        .filter(|r| r.at < hour_ago || since.is_none_or(|s| r.at >= s))
        .cloned()
        .partition(|r| r.at >= hour_ago);
    // Its usual is per hour of the history it has, not of a week it hasn't.
    let first = before.iter().map(|r| r.at).min();
    let hours = first.map_or(0.0, |f| {
        ((hour_ago - f).num_minutes() as f64 / 60.0).clamp(1.0, USUAL_HOURS as f64)
    });
    let hour_use = work_use::fold(hour);
    let before_use = work_use::fold(before);
    let longest = |u: &[WorkUse]| {
        u.first()
            .and_then(|u| u.longest_share().cloned())
            .map(|s| (s.plan.clone(), s.window_minutes))
    };
    let of = longest(&hour_use).or_else(|| longest(&before_use));
    let share_of = |u: &[WorkUse]| {
        u.first()
            .and_then(|u| {
                of.as_ref().and_then(|(plan, minutes)| {
                    u.shares
                        .iter()
                        .find(|s| s.plan == *plan && s.window_minutes == *minutes)
                })
            })
            .map_or(0.0, |s| s.share)
    };
    let turns_of = |u: &[WorkUse]| u.first().map_or(0, |u| u.turns);
    Rate {
        hour_share: share_of(&hour_use),
        usual_share: if hours > 0.0 {
            share_of(&before_use) / hours
        } else {
            0.0
        },
        hour_turns: turns_of(&hour_use),
        usual_turns: if hours > 0.0 {
            turns_of(&before_use) as f64 / hours
        } else {
            0.0
        },
        of: of.and_then(|(plan, minutes)| PlanKind::parse(&plan).map(|p| (p, minutes))),
    }
}

/// Why `rate` makes `work` a runaway, if it does.
fn runaway(cfg: &PlanGuardConfig, work: &Work, rate: &Rate, limit: Option<f64>) -> Option<String> {
    let multiple = cfg.runaway_multiple.max(1.0);
    let of = rate
        .of
        .map(|(plan, minutes)| format!("your {} {} plan", window_words(minutes), plan_words(plan)));
    let what = work_words(work);
    let share = |p: f64| format!("{}%", (p * 10.0).round() / 10.0);
    let times = |now: f64, usual: f64| {
        if usual > 0.0 {
            format!(", about {}× its usual", (now / usual).round() as i64)
        } else {
            String::new()
        }
    };
    let by_share = match limit {
        Some(limit) => rate.hour_share >= limit.max(0.0) && rate.hour_share > 0.0,
        None => {
            rate.hour_share >= cfg.runaway_min_percent.max(0.0)
                && rate.hour_share > 0.0
                && (rate.usual_share <= 0.0 || rate.hour_share >= multiple * rate.usual_share)
        }
    };
    if by_share {
        let of = of.unwrap_or_else(|| "your plan".into());
        let past = match limit {
            Some(limit) => format!(", past the {} an hour you allowed it", share(limit)),
            None => times(rate.hour_share, rate.usual_share),
        };
        return Some(format!(
            "{what} used {} of {of} in the last hour{past}. I've paused it.",
            share(rate.hour_share)
        ));
    }
    let floor = u64::from(cfg.runaway_turns_per_hour);
    let by_turns = floor > 0
        && rate.hour_turns >= floor
        && (rate.usual_turns <= 0.0 || rate.hour_turns as f64 >= multiple * rate.usual_turns);
    by_turns.then(|| {
        format!(
            "{what} began {} turns in the last hour{}. I've paused it.",
            rate.hour_turns,
            times(rate.hour_turns as f64, rate.usual_turns)
        )
    })
}

/// "Your 'email check' job", "Atlas's own turns".
pub fn work_words(work: &Work) -> String {
    use workforce_common::work_use::WorkKind;
    match work.kind() {
        WorkKind::Job => format!("Your '{}' job", work.label),
        WorkKind::Monitor => format!("Your '{}' monitor", work.label),
        _ => work.label.clone(),
    }
}

/// What the person can do about a stopped runaway, in words an agent acts on.
pub fn stopped_choices(work: &Work) -> String {
    use workforce_common::work_use::WorkKind;
    match work.kind() {
        WorkKind::Job => "Resume it, keep it paused, or raise its limit (say how much of the plan an hour it may use).".into(),
        WorkKind::Monitor => "Turn it back on, keep it off, or change what it watches.".into(),
        _ => "It goes on by itself in an hour, or when you say so.".into(),
    }
}

/// What a recurring job would use of the person's plan, said before it is
/// made or made to run more often.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PlanEstimate {
    /// In words: "Runs about 144 times a day ≈ 4% of your weekly Claude plan
    /// a day (28% a week), going by its past runs."
    pub text: String,
    /// Points of the plan's longest window a day, when there is anything to
    /// go by.
    pub per_day_percent: Option<f64>,
    pub window_minutes: Option<u32>,
    /// At least `[budget.plan] notable_daily_percent` a day: say so.
    pub expensive: bool,
}

impl PlanEstimate {
    /// From a week of `rows`: the share a run of `job_id` took (a turn is a
    /// run), else a run of any of `agent`'s jobs, times `runs_per_day`.
    pub fn of(
        rows: &[WorkUseRow],
        plan: PlanKind,
        agent: &str,
        job_id: Option<&str>,
        runs_per_day: f64,
        notable: f64,
    ) -> Self {
        let ours = |r: &&WorkUseRow| r.plan.as_deref().is_none_or(|p| p == plan.as_str());
        let own = job_id.map(|id| format!("job:{id}"));
        let of_job: Vec<WorkUseRow> = rows
            .iter()
            .filter(ours)
            .filter(|r| Some(&r.work) == own.as_ref())
            .cloned()
            .collect();
        let of_agent: Vec<WorkUseRow> = rows
            .iter()
            .filter(ours)
            .filter(|r| r.agent_id == agent && r.work.starts_with("job:"))
            .cloned()
            .collect();
        let per_run = |rows: Vec<WorkUseRow>| -> Option<(u32, f64)> {
            let used = work_use::fold(rows);
            let turns: u64 = used.iter().map(|u| u.turns).sum();
            let longest = used
                .iter()
                .flat_map(|u| u.shares.iter())
                .map(|s| s.window_minutes)
                .max()?;
            let share: f64 = used
                .iter()
                .flat_map(|u| u.shares.iter())
                .filter(|s| s.window_minutes == longest)
                .map(|s| s.share)
                .sum();
            (turns > 0).then(|| (longest, share / turns as f64))
        };
        let runs = match runs_per_day {
            r if r >= 10.0 => format!("about {r:.0} times a day"),
            r if r >= 1.0 => format!("about {r:.1} times a day"),
            r => format!("about {:.1} times a week", r * 7.0),
        };
        let (from, found) = match per_run(of_job) {
            Some(found) => ("its past runs", Some(found)),
            None => ("this agent's other jobs", per_run(of_agent)),
        };
        let Some((minutes, share)) = found else {
            return Self {
                text: format!(
                    "Runs {runs} on your {} plan; there are no past runs to say how much of it yet.",
                    plan_words(plan)
                ),
                per_day_percent: None,
                window_minutes: None,
                expensive: false,
            };
        };
        let per_day = share * runs_per_day;
        let window = window_words(minutes);
        let mut text = format!(
            "Runs {runs} ≈ {} of your {window} {} plan a day",
            percent(per_day),
            plan_words(plan)
        );
        if minutes >= 2 * 1_440 {
            let days = f64::from(minutes) / 1_440.0;
            text.push_str(&format!(
                " ({} a {})",
                percent(per_day * days),
                match minutes {
                    10_080 => "week".to_string(),
                    _ => format!("{window} window"),
                }
            ));
        }
        text.push_str(&format!(", going by {from}."));
        Self {
            text,
            per_day_percent: Some(per_day),
            window_minutes: Some(minutes),
            expensive: per_day >= notable,
        }
    }
}

fn percent(p: f64) -> String {
    match p {
        p if p < 0.1 => "under 0.1%".into(),
        p if p < 10.0 => format!("{p:.1}%"),
        p => format!("{p:.0}%"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_jobs_estimate_goes_by_its_past_runs_else_the_agents_other_jobs() {
        let now = Utc::now();
        let old = Work::job("atlas", "j1", "email check");
        let mut rows = Vec::new();
        for _ in 0..10 {
            rows.push(WorkUseRow::turn(&old, Some("p"), now));
            rows.push(WorkUseRow::share(
                &old,
                Some("p"),
                "claude",
                10_080,
                0.03,
                now,
            ));
            rows.push(WorkUseRow::share(&old, Some("p"), "claude", 300, 0.5, now));
        }
        // Every 10 minutes, from the agent's other job: 0.03 a run.
        let new = PlanEstimate::of(&rows, PlanKind::Claude, "atlas", None, 144.0, 5.0);
        assert_eq!(
            new.text,
            "Runs about 144 times a day ≈ 4.3% of your weekly Claude plan a day (30% a week), \
             going by this agent's other jobs."
        );
        assert!(!new.expensive);
        let busy = PlanEstimate::of(&rows, PlanKind::Claude, "atlas", Some("j1"), 288.0, 5.0);
        assert!(
            busy.text.ends_with("going by its past runs."),
            "{}",
            busy.text
        );
        assert!(busy.expensive);
        let none = PlanEstimate::of(&[], PlanKind::Chatgpt, "atlas", None, 0.5, 5.0);
        assert_eq!(
            none.text,
            "Runs about 3.5 times a week on your ChatGPT plan; there are no past runs to say how much of it yet."
        );
        assert_eq!(none.per_day_percent, None);
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    fn login(token: &str) -> Login {
        super::super::logins::test_support::claude_login(token)
    }

    fn week(used: f64, reset: &str) -> Window {
        Window {
            used_percent: used,
            resets_at: Some(at(reset)),
            minutes: 10_080,
        }
    }

    fn cfg() -> PlanGuardConfig {
        PlanGuardConfig::default()
    }

    #[test]
    fn background_waits_past_the_threshold_and_goes_on_once_under_or_reset() {
        let guard = PlanGuard::new();
        let l = login("aa");
        let job = Work::job("atlas", "j1", "email check");
        let now = at("2026-11-19T10:00:00Z");
        assert_eq!(
            guard.check(&cfg(), &job, Some(&l), now),
            Verdict::Proceed,
            "nothing read: it runs"
        );
        guard.observe(&l, &[week(83.0, "2026-11-23T09:00:00Z")], now);
        match guard.check(&cfg(), &job, Some(&l), now) {
            Verdict::Waits(why) => {
                assert!(why.contains("your weekly Claude plan is at 83%"), "{why}");
                assert!(why.contains("pauses from 80%"), "{why}");
            }
            other => panic!("{other:?}"),
        }
        let mut lower = cfg();
        lower.pause_at_percent = 90.0;
        assert_eq!(
            guard.check(&lower, &job, Some(&l), now),
            Verdict::Proceed,
            "configurable"
        );
        // Past the reset, with nothing read since: it goes on.
        let after = at("2026-11-23T09:00:01Z");
        assert_eq!(guard.check(&cfg(), &job, Some(&l), after), Verdict::Proceed);
        // A newer, lower reading frees it too.
        guard.observe(
            &l,
            &[week(40.0, "2026-11-30T09:00:00Z")],
            at("2026-11-23T10:00:00Z"),
        );
        assert_eq!(
            guard.check(&cfg(), &job, Some(&l), at("2026-11-23T10:00:00Z")),
            Verdict::Proceed
        );
        assert_eq!(
            guard.check(&cfg(), &job, None, now),
            Verdict::Proceed,
            "not on a plan"
        );
        let mut off = cfg();
        off.enabled = false;
        guard.observe(
            &l,
            &[week(99.0, "2026-11-30T09:00:00Z")],
            at("2026-11-23T11:00:00Z"),
        );
        assert_eq!(
            guard.check(&off, &job, Some(&l), at("2026-11-23T11:00:00Z")),
            Verdict::Proceed
        );
    }

    #[test]
    fn a_reading_older_than_the_one_kept_never_replaces_it() {
        let guard = PlanGuard::new();
        let l = login("bb");
        guard.observe(
            &l,
            &[week(85.0, "2026-11-23T09:00:00Z")],
            at("2026-11-19T10:00:00Z"),
        );
        guard.observe(
            &l,
            &[week(10.0, "2026-11-23T09:00:00Z")],
            at("2026-11-19T09:00:00Z"),
        );
        let s = guard.standing(&l, at("2026-11-19T10:00:00Z")).unwrap();
        assert_eq!(s.used_percent, 85.0);
    }

    #[test]
    fn what_a_call_moved_is_its_own_and_the_persons_use_between_is_not() {
        let guard = PlanGuard::new();
        let store: Arc<dyn TelemetryStore> =
            Arc::new(workforce_common::persistence::SqliteStore::in_memory().unwrap());
        guard.keep_in(Arc::clone(&store));
        let l = login("cc");
        let reset = "2026-11-23T09:00:00Z";
        guard.observe(&l, &[week(10.0, reset)], at("2026-11-19T09:00:00Z"));
        let job = Work::job("atlas", "j1", "email check");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(work_use::within(job.clone(), async {
            guard.observe(&l, &[week(12.0, reset)], at("2026-11-19T09:05:00Z"));
            guard.served(&l, "p1");
        }));
        // An interactive call moves it too: never the job's.
        guard.observe(&l, &[week(15.0, reset)], at("2026-11-19T09:06:00Z"));
        guard.served(&l, "p1");
        let rows = store
            .work_use_rows(at("2026-01-01T00:00:00Z"), Utc::now(), None, Some(&job.key))
            .unwrap();
        let used = work_use::fold(rows);
        assert_eq!(used[0].longest_share().unwrap().share, 2.0);
    }

    fn rows_for(
        job: &Work,
        hour_share: f64,
        hour_turns: u32,
        now: DateTime<Utc>,
    ) -> Vec<WorkUseRow> {
        let mut rows = Vec::new();
        // A week of 0.1 points and 6 turns an hour.
        for h in 2..=(7 * 24) {
            let t = now - chrono::Duration::hours(h);
            rows.push(WorkUseRow::share(job, Some("p"), "claude", 10_080, 0.1, t));
            for _ in 0..6 {
                rows.push(WorkUseRow::turn(job, Some("p"), t));
            }
        }
        let t = now - chrono::Duration::minutes(10);
        rows.push(WorkUseRow::share(
            job,
            Some("p"),
            "claude",
            10_080,
            hour_share,
            t,
        ));
        for _ in 0..hour_turns {
            rows.push(WorkUseRow::turn(job, Some("p"), t));
        }
        rows
    }

    fn rate_of_all(rows: &[WorkUseRow], now: DateTime<Utc>) -> Rate {
        rate_of(rows, None, now)
    }

    #[test]
    fn a_job_using_far_more_than_its_usual_is_a_runaway_said_in_plain_words() {
        let job = Work::job("atlas", "j1", "email check");
        let now = at("2026-11-19T10:00:00Z");
        let rate = rate_of_all(&rows_for(&job, 4.0, 6, now), now);
        assert!((rate.usual_share - 0.1).abs() < 0.01, "{rate:?}");
        let why = runaway(&cfg(), &job, &rate, None).unwrap();
        assert_eq!(
            why,
            "Your 'email check' job used 4% of your weekly Claude plan in the last hour, about 40× its usual. I've paused it."
        );
        let usual = rate_of_all(&rows_for(&job, 0.1, 6, now), now);
        assert!(runaway(&cfg(), &job, &usual, None).is_none());
        // Under the floor nothing is a runaway, however far past its usual.
        let small = rate_of_all(&rows_for(&job, 2.0, 6, now), now);
        assert!(runaway(&cfg(), &job, &small, None).is_none());
        // A limit the person raised replaces the usual.
        assert!(runaway(&cfg(), &job, &rate, Some(5.0)).is_none());
        let past = runaway(&cfg(), &job, &rate, Some(3.0)).unwrap();
        assert!(
            past.contains("past the 3% an hour you allowed it"),
            "{past}"
        );
        // Turns alone: a loop that moves nothing measurable.
        let looping = rate_of_all(&rows_for(&job, 0.0, 200, now), now);
        let why = runaway(&cfg(), &job, &looping, None).unwrap();
        assert!(
            why.contains("began 200 turns in the last hour, about 33× its usual"),
            "{why}"
        );
    }

    #[test]
    fn a_runaway_is_told_once_and_stays_stopped_until_released() {
        let guard = PlanGuard::new();
        let store: Arc<dyn TelemetryStore> =
            Arc::new(workforce_common::persistence::SqliteStore::in_memory().unwrap());
        guard.keep_in(Arc::clone(&store));
        let job = Work::job("atlas", "j1", "email check");
        for _ in 0..130 {
            guard.began(&job, Some("p"));
        }
        let now = Utc::now();
        let first = guard.check(&cfg(), &job, None, now);
        assert!(
            matches!(first, Verdict::Stopped { first: true, .. }),
            "{first:?}"
        );
        let again = guard.check(&cfg(), &job, None, now);
        assert!(
            matches!(again, Verdict::Stopped { first: false, .. }),
            "{again:?}"
        );
        // Resumed: what it did before is not held against it.
        guard.release(&job.key);
        assert_eq!(
            guard.check(&cfg(), &job, None, Utc::now()),
            Verdict::Proceed
        );
        // A job resumed (kept on the job itself, across restarts) likewise.
        let other = PlanGuard::new();
        other.keep_in(Arc::clone(&store));
        let resumed = job.clone().allowing(Allowed {
            resumed_at: Some(Utc::now()),
            ..Allowed::default()
        });
        assert_eq!(
            other.check(&cfg(), &resumed, None, Utc::now()),
            Verdict::Proceed
        );
        assert!(matches!(
            other.check(&cfg(), &job, None, Utc::now()),
            Verdict::Stopped { first: true, .. }
        ));
        // The same guard, the job turned back on after it was stopped: it
        // goes on at once.
        let back_on = job.clone().allowing(Allowed {
            resumed_at: Some(Utc::now() + chrono::Duration::seconds(1)),
            ..Allowed::default()
        });
        assert_eq!(
            other.check(
                &cfg(),
                &back_on,
                None,
                Utc::now() + chrono::Duration::seconds(2)
            ),
            Verdict::Proceed
        );
        // A stop nothing else ends lapses after an hour.
        assert!(other
            .stopped(&job.key, None, Utc::now() + chrono::Duration::minutes(61))
            .is_none());
    }

    #[test]
    fn windows_are_named_as_the_person_says_them() {
        assert_eq!(window_words(10_080), "weekly");
        assert_eq!(window_words(300), "five-hour");
        assert_eq!(window_words(180), "3-hour");
        assert_eq!(window_words(2_880), "2-day");
    }
}
