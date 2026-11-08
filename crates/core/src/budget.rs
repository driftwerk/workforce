//! Budgets the person sets on Desk: what each applies to, the periods they
//! count over, and where a subject stands against them. Pure: the desk keeps
//! them and its breaker acts on them for sessions; the hub's cost governor
//! acts on them for agents.

use chrono::{DateTime, Datelike, Duration, Months, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// What a budget counts over. Serialized in snake case (`five_hours`), as
/// its text is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    FiveHours,
    Day,
    Week,
    Month,
    /// Everything ever spent toward it, in no period: a plan's whole run.
    Total,
}

/// Where a [`Period::Total`] budget counts from: everything.
fn ever() -> DateTime<Utc> {
    DateTime::UNIX_EPOCH
}

/// Where a [`Period::Total`] budget's period ends: never, in practice.
fn never() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2200-01-01T00:00:00Z")
        .map(|t| t.to_utc())
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

impl Period {
    pub const ALL: [Period; 5] = [
        Self::FiveHours,
        Self::Day,
        Self::Week,
        Self::Month,
        Self::Total,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::FiveHours => "five_hours",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Total => "total",
        }
    }

    pub fn parse(s: &str) -> eyre::Result<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.as_str() == s)
            .ok_or_else(|| eyre::eyre!("unknown Period {s:?}"))
    }

    /// As a sentence names it: "day", "five hours".
    pub fn words(self) -> &'static str {
        match self {
            Self::FiveHours => "five-hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Total => "total",
        }
    }
}

crate::text_enum!(Unit {
    Tokens => "tokens",
    Usd => "usd",
});

crate::text_enum!(OnExceed {
    // Held from its next turn (sessions) or its next turn refused
    // (agents), until the person says Continue.
    Pause => "pause",
    // Only told: nothing is held.
    Warn => "warn",
});

/// Where a period began in `tz`: five hours ago, local midnight, Monday's
/// midnight, the 1st's. A midnight a clock change skips is its first
/// instant after.
pub fn window_start(period: Period, now: DateTime<Utc>, tz: Tz) -> DateTime<Utc> {
    let local = now.with_timezone(&tz).date_naive();
    let day = match period {
        Period::FiveHours => return now - Duration::hours(5),
        Period::Total => return ever(),
        Period::Day => local,
        Period::Week => local - Duration::days(i64::from(local.weekday().num_days_from_monday())),
        Period::Month => local.with_day(1).unwrap_or(local),
    };
    midnight(day, tz).unwrap_or(now - Duration::hours(24))
}

/// Where the period `now` is in ends: five hours from now (a rolling
/// window has no end of its own), the next local midnight, next Monday's,
/// the next 1st's.
pub fn window_end(period: Period, now: DateTime<Utc>, tz: Tz) -> DateTime<Utc> {
    let local = now.with_timezone(&tz).date_naive();
    let day = match period {
        Period::FiveHours => return now + Duration::hours(5),
        Period::Total => return never(),
        Period::Day => local + Duration::days(1),
        Period::Week => {
            local + Duration::days(7 - i64::from(local.weekday().num_days_from_monday()))
        }
        Period::Month => local
            .with_day(1)
            .and_then(|d| d.checked_add_months(Months::new(1)))
            .unwrap_or(local + Duration::days(31)),
    };
    midnight(day, tz).unwrap_or(now + Duration::hours(24))
}

fn midnight(day: NaiveDate, tz: Tz) -> Option<DateTime<Utc>> {
    (0..4).find_map(|hour| {
        day.and_hms_opt(hour, 0, 0)?
            .and_local_timezone(tz)
            .earliest()
            .map(|t| t.with_timezone(&Utc))
    })
}

/// What a budget applies to. `Desk` with no group is every session and
/// agent on Desk together; with a group, that group's together. `Each*` is
/// the default for each one that has no budget of its own: a group's for
/// its members, else the ungrouped one. A session's or agent's own budget
/// is keyed by it, so it goes wherever it is moved. A plan's is what the
/// person approved for it, counted over every session working on its
/// tickets, on top of whatever else applies to each.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum BudgetScope {
    Desk,
    EachSession,
    EachAgent,
    Session(Uuid),
    Agent(String),
    Plan(Uuid),
}

impl BudgetScope {
    /// As the `budgets` table keeps it.
    pub fn text(&self) -> String {
        match self {
            Self::Desk => "desk".into(),
            Self::EachSession => "each_session".into(),
            Self::EachAgent => "each_agent".into(),
            Self::Session(id) => format!("session:{id}"),
            Self::Agent(id) => format!("agent:{id}"),
            Self::Plan(id) => format!("plan:{id}"),
        }
    }

    pub fn parse(s: &str) -> eyre::Result<Self> {
        Ok(match s {
            "desk" => Self::Desk,
            "each_session" => Self::EachSession,
            "each_agent" => Self::EachAgent,
            other => match other.split_once(':') {
                Some(("session", id)) => Self::Session(Uuid::parse_str(id)?),
                Some(("plan", id)) => Self::Plan(Uuid::parse_str(id)?),
                Some(("agent", id)) if !id.is_empty() => Self::Agent(id.to_string()),
                _ => eyre::bail!("unknown budget scope {other:?}"),
            },
        })
    }

    /// Whether it is one item's own: it carries no group.
    pub fn own(&self) -> bool {
        matches!(self, Self::Session(_) | Self::Agent(_))
    }
}

/// One budget, as the person set it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Budget {
    pub id: Uuid,
    pub scope: BudgetScope,
    /// The sidebar group it is the budget of; none is ungrouped (and for an
    /// item's own budget, always none).
    pub group: Option<String>,
    pub period: Period,
    pub unit: Unit,
    pub amount: f64,
    /// The share of `amount` at which it warns, between 0 and 1.
    pub warn_at: f64,
    pub on_exceed: OnExceed,
    pub updated_at: DateTime<Utc>,
}

/// The default share at which a budget warns.
pub const WARN_AT: f64 = 0.8;

/// Why a budget can't be kept, in plain words.
pub fn check(b: &Budget) -> Result<(), String> {
    if !(b.amount.is_finite() && b.amount > 0.0) {
        return Err("a budget's amount is more than nothing".into());
    }
    if !(b.warn_at > 0.0 && b.warn_at < 1.0) {
        return Err("a budget warns somewhere between 1% and 99% of it".into());
    }
    if b.scope.own() && b.group.is_some() {
        return Err("a session's or agent's own budget goes with it, not with a group".into());
    }
    match (&b.scope, b.period) {
        (BudgetScope::Plan(_), _) => {}
        (_, Period::Total) => {
            return Err("only a plan's budget counts over its whole run".into());
        }
        _ => {}
    }
    if b.unit == Unit::Tokens && b.amount.fract() != 0.0 {
        return Err("a token budget is a whole number of tokens".into());
    }
    Ok(())
}

/// One session or agent a budget can apply to, with the group it is in now
/// and, for a session, the plan whose ticket it works on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject<'a> {
    Session {
        id: Uuid,
        group: Option<&'a str>,
        plan: Option<Uuid>,
    },
    Agent {
        id: &'a str,
        group: Option<&'a str>,
    },
}

/// The budgets that apply to `subject` alone: its own, else its group's
/// `each_*` default, else the ungrouped one; and, on top, for a session
/// working on a plan's ticket, the plan's (counted over the plan's whole
/// spend, not the session's). Totals (`Desk`) are pools, read by [`pools`].
pub fn applying<'a>(all: &'a [Budget], subject: &Subject<'_>) -> Vec<&'a Budget> {
    let mut out = alone(all, subject);
    if let Subject::Session { plan: Some(p), .. } = subject {
        let plan = BudgetScope::Plan(*p);
        out.extend(all.iter().filter(|b| b.scope == plan));
    }
    out
}

/// The budgets of `subject`'s own: its own, else its group's default, else
/// the ungrouped one.
fn alone<'a>(all: &'a [Budget], subject: &Subject<'_>) -> Vec<&'a Budget> {
    let (own, each, group) = match subject {
        Subject::Session { id, group, .. } => {
            (BudgetScope::Session(*id), BudgetScope::EachSession, *group)
        }
        Subject::Agent { id, group } => (
            BudgetScope::Agent((*id).to_string()),
            BudgetScope::EachAgent,
            *group,
        ),
    };
    let of = |scope: &BudgetScope, g: Option<&str>| -> Vec<&'a Budget> {
        all.iter()
            .filter(|b| b.scope == *scope && b.group.as_deref() == g)
            .collect()
    };
    let mine = of(&own, None);
    if !mine.is_empty() {
        return mine;
    }
    if let Some(g) = group {
        let theirs = of(&each, Some(g));
        if !theirs.is_empty() {
            return theirs;
        }
    }
    of(&each, None)
}

/// A pool of spend counted together: everything on Desk, or one group.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", content = "group", rename_all = "snake_case")]
pub enum Pool {
    Desk,
    Group(String),
}

impl Pool {
    /// The group it is, if one.
    pub fn group(&self) -> Option<&str> {
        match self {
            Self::Desk => None,
            Self::Group(g) => Some(g),
        }
    }
}

/// The pools an item in `group` counts toward: Desk's, and its group's.
pub fn pools_of(group: Option<&str>) -> Vec<Pool> {
    std::iter::once(Pool::Desk)
        .chain(group.map(|g| Pool::Group(g.to_string())))
        .collect()
}

/// The total budgets of `pool`.
pub fn pools<'a>(all: &'a [Budget], pool: &Pool) -> Vec<&'a Budget> {
    all.iter()
        .filter(|b| b.scope == BudgetScope::Desk && b.group.as_deref() == pool.group())
        .collect()
}

/// Where spend stands against one budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Under,
    Warn,
    Over,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Standing {
    pub budget: Uuid,
    pub period: Period,
    pub unit: Unit,
    pub used: f64,
    pub of: f64,
    pub status: Status,
    pub on_exceed: OnExceed,
    /// Where its period began.
    pub from: DateTime<Utc>,
    /// Where its period ends.
    pub to: DateTime<Utc>,
    /// Counted over many sessions together (a plan's), not the subject's
    /// own spend alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shared: bool,
}

impl Standing {
    /// How much of it is used, as a share.
    pub fn share(&self) -> f64 {
        if self.of > 0.0 {
            self.used / self.of
        } else {
            0.0
        }
    }

    /// "83%".
    pub fn percent(&self) -> String {
        // Floored, past float noise: 1.15 of 1 is 115%, not 114%.
        format!("{}%", (self.share() * 100.0 + 1e-6).floor() as i64)
    }

    /// "$9.10 of $11", "410k of 500k tokens".
    pub fn amounts(&self) -> String {
        match self.unit {
            Unit::Usd => format!("{} of {}", usd(self.used), usd(self.of)),
            Unit::Tokens => format!("{} of {} tokens", tokens(self.used), tokens(self.of)),
        }
    }
}

/// "$4.10", "$11".
pub fn usd(v: f64) -> String {
    if v.fract().abs() < 0.005 {
        format!("${v:.0}")
    } else {
        format!("${v:.2}")
    }
}

/// "830", "34k", "1.2M".
pub fn tokens(v: f64) -> String {
    let short = |d: f64, s: &str| {
        let x = v / d;
        if x >= 100.0 {
            format!("{}{s}", x.round())
        } else {
            format!("{}{s}", (x * 10.0).round() / 10.0)
        }
    };
    match v {
        v if v < 1e3 => format!("{}", v.round()),
        v if v < 1e6 => short(1e3, "k"),
        v if v < 1e9 => short(1e6, "M"),
        _ => short(1e9, "B"),
    }
}

/// Where `used` stands against `b`, in its period `from`..`to`.
pub fn standing_of(b: &Budget, used: f64, from: DateTime<Utc>, to: DateTime<Utc>) -> Standing {
    let status = if used >= b.amount {
        Status::Over
    } else if used >= b.amount * b.warn_at {
        Status::Warn
    } else {
        Status::Under
    };
    Standing {
        budget: b.id,
        period: b.period,
        unit: b.unit,
        used,
        of: b.amount,
        status,
        on_exceed: b.on_exceed,
        from,
        to,
        shared: false,
    }
}

/// The tightest of `standings`: the furthest along, over before warned.
pub fn tightest(standings: impl IntoIterator<Item = Standing>) -> Option<Standing> {
    standings.into_iter().max_by(|a, b| {
        a.status
            .cmp(&b.status)
            .then(a.share().total_cmp(&b.share()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    fn budget(scope: BudgetScope, group: Option<&str>, amount: f64) -> Budget {
        Budget {
            id: Uuid::new_v4(),
            scope,
            group: group.map(str::to_string),
            period: Period::Day,
            unit: Unit::Usd,
            amount,
            warn_at: WARN_AT,
            on_exceed: OnExceed::Pause,
            updated_at: at("2026-11-03T09:00:00Z"),
        }
    }

    #[test]
    fn periods_end_at_the_next_local_midnight_across_a_clock_change_and_a_year_end() {
        let london: Tz = "Europe/London".parse().unwrap();
        assert_eq!(
            window_end(Period::Day, at("2026-10-24T12:00:00Z"), london),
            at("2026-10-24T23:00:00Z"),
            "summer time"
        );
        assert_eq!(
            window_end(Period::Day, at("2026-10-25T12:00:00Z"), london),
            at("2026-10-26T00:00:00Z"),
            "the clocks went back that morning"
        );
        assert_eq!(
            window_end(Period::Week, at("2026-11-04T12:00:00Z"), london),
            at("2026-11-09T00:00:00Z"),
            "next Monday"
        );
        assert_eq!(
            window_end(Period::Month, at("2026-12-15T12:00:00Z"), london),
            at("2027-01-01T00:00:00Z")
        );
        let now = at("2026-11-03T12:00:00Z");
        assert_eq!(
            window_end(Period::FiveHours, now, london),
            at("2026-11-03T17:00:00Z")
        );
        for p in Period::ALL {
            assert!(window_start(p, now, london) <= now && now < window_end(p, now, london));
            assert_eq!(Period::parse(p.as_str()).unwrap(), p);
        }
        assert_eq!(
            serde_json::to_value(Period::FiveHours).unwrap(),
            "five_hours"
        );
    }

    #[test]
    fn scopes_round_trip_as_kept() {
        let id = Uuid::new_v4();
        for s in [
            BudgetScope::Desk,
            BudgetScope::EachSession,
            BudgetScope::EachAgent,
            BudgetScope::Session(id),
            BudgetScope::Agent("atlas".into()),
            BudgetScope::Plan(id),
        ] {
            assert_eq!(BudgetScope::parse(&s.text()).unwrap(), s);
        }
        assert!(BudgetScope::parse("agent:").is_err());
        assert!(BudgetScope::parse("session:nope").is_err());
        assert_eq!(
            serde_json::to_value(BudgetScope::Session(id)).unwrap(),
            serde_json::json!({ "kind": "session", "id": id })
        );
        assert_eq!(
            serde_json::to_value(BudgetScope::Desk).unwrap(),
            serde_json::json!({ "kind": "desk" })
        );
    }

    #[test]
    fn an_items_own_budget_replaces_its_groups_default_which_replaces_the_ungrouped_one() {
        let id = Uuid::new_v4();
        let each = budget(BudgetScope::EachSession, None, 5.0);
        let acme_each = budget(BudgetScope::EachSession, Some("acme"), 3.0);
        let own = budget(BudgetScope::Session(id), None, 1.0);
        let total = budget(BudgetScope::Desk, None, 50.0);
        let ids = |v: Vec<&Budget>| v.iter().map(|b| b.id).collect::<Vec<_>>();

        let all = vec![each.clone(), acme_each.clone(), total.clone()];
        let s = |group| Subject::Session {
            id,
            group,
            plan: None,
        };
        assert_eq!(ids(applying(&all, &s(None))), [each.id]);
        assert_eq!(ids(applying(&all, &s(Some("acme")))), [acme_each.id]);
        assert_eq!(ids(applying(&all, &s(Some("atlas")))), [each.id]);

        let mut with_own = all.clone();
        with_own.push(own.clone());
        assert_eq!(
            ids(applying(&with_own, &s(Some("acme")))),
            [own.id],
            "it moves with the session"
        );
        assert_eq!(
            ids(applying(
                &all,
                &Subject::Agent {
                    id: "atlas",
                    group: None
                }
            )),
            Vec::<Uuid>::new(),
            "a session's default isn't an agent's"
        );
        assert_eq!(ids(pools(&all, &Pool::Desk)), [total.id]);
        assert!(pools(&all, &Pool::Group("acme".into())).is_empty());
        assert_eq!(
            pools_of(Some("acme")),
            [Pool::Desk, Pool::Group("acme".into())]
        );
    }

    #[test]
    fn a_groups_budgets_apply_first_then_the_ungrouped_defaults_and_the_desk_wide_limit_always() {
        let id = Uuid::new_v4();
        let each = budget(BudgetScope::EachSession, None, 5.0);
        let acme_each = budget(BudgetScope::EachSession, Some("acme"), 3.0);
        let acme_total = budget(BudgetScope::Desk, Some("acme"), 30.0);
        let desk_wide = budget(BudgetScope::Desk, None, 100.0);
        let all = vec![
            each.clone(),
            acme_each.clone(),
            acme_total.clone(),
            desk_wide.clone(),
        ];
        let ids = |v: Vec<&Budget>| v.iter().map(|b| b.id).collect::<Vec<_>>();
        let in_group = |g| Subject::Session {
            id,
            group: g,
            plan: None,
        };
        // The group's own rows, else the ungrouped defaults.
        assert_eq!(ids(applying(&all, &in_group(Some("acme")))), [acme_each.id]);
        assert_eq!(ids(applying(&all, &in_group(Some("atlas")))), [each.id]);
        // Then the pools: the group's total, and the Desk-wide limit over
        // everything, whatever group it is in.
        let pools_for = |g| {
            pools_of(g)
                .iter()
                .flat_map(|p| ids(pools(&all, p)))
                .collect::<Vec<_>>()
        };
        assert_eq!(pools_for(Some("acme")), [desk_wide.id, acme_total.id]);
        assert_eq!(pools_for(Some("atlas")), [desk_wide.id]);
        assert_eq!(pools_for(None), [desk_wide.id]);
    }

    #[test]
    fn the_tightest_standing_is_the_furthest_along_and_over_beats_warned() {
        let (from, to) = (at("2026-11-03T00:00:00Z"), at("2026-11-04T00:00:00Z"));
        let a = budget(BudgetScope::Desk, None, 10.0);
        let b = budget(BudgetScope::Desk, None, 100.0);
        assert_eq!(standing_of(&a, 7.9, from, to).status, Status::Under);
        assert_eq!(standing_of(&a, 8.0, from, to).status, Status::Warn);
        assert_eq!(standing_of(&a, 10.0, from, to).status, Status::Over);
        let t = tightest([
            standing_of(&a, 8.3, from, to),
            standing_of(&b, 99.0, from, to),
        ])
        .unwrap();
        assert_eq!(t.budget, b.id);
        assert_eq!(t.percent(), "99%");
        let t = tightest([
            standing_of(&a, 10.5, from, to),
            standing_of(&b, 99.0, from, to),
        ])
        .unwrap();
        assert_eq!(t.budget, a.id);
        assert_eq!(standing_of(&a, 9.1, from, to).amounts(), "$9.10 of $10");
        assert!(tightest([]).is_none());
    }

    #[test]
    fn a_plans_budget_applies_to_its_sessions_on_top_of_their_own_or_their_default() {
        let (id, plan, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let each = budget(BudgetScope::EachSession, None, 5.0);
        let own = budget(BudgetScope::Session(id), None, 1.0);
        let mut capped = budget(BudgetScope::Plan(plan), Some("acme"), 40.0);
        capped.period = Period::Total;
        let theirs = budget(BudgetScope::Plan(other), None, 9.0);
        let all = vec![each.clone(), own.clone(), capped.clone(), theirs];
        let ids = |v: Vec<&Budget>| v.iter().map(|b| b.id).collect::<Vec<_>>();
        let on = |id, plan| Subject::Session {
            id,
            group: Some("acme"),
            plan,
        };
        assert_eq!(
            ids(applying(&all, &on(id, Some(plan)))),
            [own.id, capped.id]
        );
        let elsewhere = Uuid::new_v4();
        assert_eq!(
            ids(applying(&all, &on(elsewhere, Some(plan)))),
            [each.id, capped.id]
        );
        assert_eq!(ids(applying(&all, &on(elsewhere, None))), [each.id]);
        assert!(
            pools(&all, &Pool::Group("acme".into())).is_empty(),
            "no pool"
        );
        assert!(check(&capped).is_ok(), "a plan's counts over its whole run");
        let mut total = budget(BudgetScope::EachSession, None, 5.0);
        total.period = Period::Total;
        assert!(check(&total).is_err(), "nothing else's does");
        let now = at("2026-11-03T12:00:00Z");
        let london: Tz = "Europe/London".parse().unwrap();
        assert!(window_start(Period::Total, now, london) < at("1971-01-01T00:00:00Z"));
        assert!(window_end(Period::Total, now, london) > at("2199-01-01T00:00:00Z"));
    }

    #[test]
    fn a_budget_is_more_than_nothing_and_an_items_own_has_no_group() {
        let mut b = budget(BudgetScope::Desk, None, 10.0);
        assert!(check(&b).is_ok());
        b.amount = 0.0;
        assert!(check(&b).is_err());
        b.amount = f64::NAN;
        assert!(check(&b).is_err());
        b.amount = 10.0;
        b.warn_at = 1.0;
        assert!(check(&b).is_err());
        let own = budget(BudgetScope::Agent("atlas".into()), Some("acme"), 10.0);
        assert!(check(&own).is_err());
        let mut t = budget(BudgetScope::Desk, None, 1.5);
        t.unit = Unit::Tokens;
        assert!(check(&t).is_err());
    }
}
