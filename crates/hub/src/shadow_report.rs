//! Shadow mode's report and weekly verdict (DW-676, PR 9c; design r2
//! sections 8 and 9): whether a shadowed store's week meets the exit
//! criteria the owner signed (D2 = M2, L1, R1), worked out from Postgres
//! alone.
//!
//! The gates are pure functions over what [`gather`] reads ([`judge`]), so
//! each rule is tested on its own:
//!
//! - **Too few to judge is a verdict of its own, never a pass.** A gate
//!   whose input is empty (no comparison on a day, no known-item run, no
//!   reconcile) says so, and so does a week under its minimum: 1,000
//!   compared live recalls of memories (below it, the memory verdict rests
//!   on the known-item runs, and says so), 200 compared artifact searches
//!   (A14).
//! - **Thresholds not set** is said, never pass, when `[vellev.shadow.gates]`
//!   holds no M2 thresholds.
//! - **L1** compares the two sides' p50 and p99 over the week, from its raw
//!   comparisons, the timed out counted at their `shadow_ms` (8.1). It is
//!   judged on live samples alone: the known-item runs ask one item at a
//!   time, and so never meet the bursts live recalls do.
//! - **A day that fails a gate restarts the count**: a week passes only
//!   with every one of its seven days passing, and its own gates too.
//! - **R1**: no unexplained reconcile fix in the week's last three days.
//!   Nothing in Postgres yet records what explains one (a bypass, a
//!   dropped note), so every fix counts as unexplained.
//!
//! What this report cannot judge from Postgres is listed in
//! [`Report::not_judged`], never counted as passed.

use chrono::{DateTime, Datelike, Duration as Days, NaiveDate, Utc};
use eyre::{Context, Result};
use serde_json::json;
use workforce_common::settings::{AgreementGates, SelfFirstGate, WorkforceConfig};
use workforce_common::shadow_postgres::{
    day_start, Breakdown, KnownItemSummary, Population, QueryMode, ShadowStore, WindowStats,
};

/// Compared live recalls of memories a week needs for its agreement to
/// rest on them; below it, it rests on the known-item runs (8.2).
pub const MEMORY_WEEK_MINIMUM: i64 = 1000;
/// Compared artifact searches a week needs, or it is too few to judge
/// (A14).
pub const ARTIFACT_WEEK_MINIMUM: i64 = 200;
/// Errors and timeouts, as a share of comparisons, at most (8.2).
pub const MOST_ERRORS: f64 = 0.005;
/// Answers empty on vellev alone, as a share of those answered, at most.
pub const MOST_EMPTY_ON_VELLEV: f64 = 0.005;
/// How far below Weaviate's artifact known-item recall@10 vellev's may be.
pub const ARTIFACT_RECALL_SLACK: f64 = 0.02;
/// How far below Weaviate's nDCG@5 vellev's may be, when judged.
pub const ARTIFACT_NDCG_SLACK: f64 = 0.02;
/// Judged queries a week needs before nDCG is a gate.
pub const JUDGED_WEEK_MINIMUM: i64 = 300;
/// Live recalls, or items both answered in a gating group, a day needs
/// before its agreement is judged; below it the day's figures are
/// information and the day neither passes nor restarts the count.
pub const DAY_M2_MINIMUM: i64 = 100;
/// Artifact known items both answered a week (window mode, serving alpha)
/// before recall@10 is judged.
pub const ARTIFACT_KNOWN_MINIMUM: i64 = 300;
/// The one-sided 95 % normal quantile: the Wilson bound's and the self@1
/// test's.
pub const Z95: f64 = 1.6449;
/// The last days of the week R1 looks at.
pub const R1_DAYS: i64 = 3;
/// Days in a week.
pub const WEEK_DAYS: i64 = 7;

/// A gate's verdict, from best to worst: a set of them is as bad as its
/// worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// Not judged here (a criterion whose input is not recorded): listed,
    /// never a pass, and it decides nothing.
    NotJudged,
    Pass,
    TooFew,
    NotSet,
    Fail,
}

impl Verdict {
    /// As `shadow_verdicts.verdict` keeps it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotJudged => "not_judged",
            Self::Pass => "pass",
            Self::TooFew => "too_few",
            Self::NotSet => "not_set",
            Self::Fail => "fail",
        }
    }

    /// As a person reads it.
    pub fn told(self) -> &'static str {
        match self {
            Self::NotJudged => "not judged",
            Self::Pass => "pass",
            Self::TooFew => "too few to judge",
            Self::NotSet => "thresholds not set",
            Self::Fail => "fail",
        }
    }
}

/// One gate, judged.
#[derive(Debug, Clone, PartialEq)]
pub struct Gate {
    pub name: &'static str,
    pub verdict: Verdict,
    pub detail: String,
}

fn gate(name: &'static str, verdict: Verdict, detail: impl Into<String>) -> Gate {
    Gate {
        name,
        verdict,
        detail: detail.into(),
    }
}

/// The worst of `verdicts`; pass when there are none.
pub fn worst(verdicts: impl IntoIterator<Item = Verdict>) -> Verdict {
    verdicts.into_iter().max().unwrap_or(Verdict::Pass)
}

/// One fix a reconcile made, and what explains it, if anything does.
#[derive(Debug, Clone, PartialEq)]
pub struct Fix {
    pub at: DateTime<Utc>,
    pub memory_id: Option<String>,
    pub explained: Option<String>,
}

/// The reconciles of the week's last [`R1_DAYS`] days.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reconciles {
    /// Those that ran to their end.
    pub ok_runs: i64,
    pub fixes: Vec<Fix>,
    /// Fixes counted but not named: none of them can be explained.
    pub fixes_left_out: i64,
    /// Whether the reconcile records a cause for each fix. Until it does,
    /// a fix is unexplained only for want of a record, and R1 is not
    /// judged.
    pub causes_recorded: bool,
}

/// One UTC day of a store's week.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Day {
    pub day: NaiveDate,
    /// Its live comparisons.
    pub live: WindowStats,
    /// Its known-item run that ran to its end, summed by population, query
    /// mode and alpha; `None` when the day has none.
    pub known: Option<Vec<KnownItemSummary>>,
    /// The serving alpha that run was asked at.
    pub run_alpha: Option<f32>,
}

/// What a store's week is judged on.
#[derive(Debug, Clone, PartialEq)]
pub struct WeekInput {
    /// `memories` or `artifacts`.
    pub store: &'static str,
    /// The week's days, in order.
    pub days: Vec<Day>,
    /// The whole week's live comparisons, from the raw rows.
    pub week: WindowStats,
    /// M2's thresholds; `None` when not set.
    pub gates: Option<AgreementGates>,
    /// Memories' reconciles over the last [`R1_DAYS`] days.
    pub reconciles: Option<Reconciles>,
    /// The self@1 test's margin and minimum sizes.
    pub self_first: SelfFirstGate,
    /// The serving alpha as configured: the known-item group that gates is
    /// the one asked at it (artifacts: 1.0).
    pub serving_alpha: f32,
    /// Distinct items both answered over the week in each gating group:
    /// an item drawn on several nights counts once. Empty when not read,
    /// and then the pairs are counted.
    pub distinct: std::collections::BTreeMap<Population, i64>,
}

/// What memories' agreement rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// At least [`MEMORY_WEEK_MINIMUM`] compared live recalls.
    Live,
    /// Fewer, and known-item runs to rest on.
    KnownItems,
    /// Fewer, and no known-item run.
    Nothing,
}

impl Basis {
    pub fn told(self) -> &'static str {
        match self {
            Self::Live => "live recalls",
            Self::KnownItems => "rests on known-item runs",
            Self::Nothing => "nothing: too few live recalls and no known-item run",
        }
    }
}

/// A day, judged.
#[derive(Debug, Clone, PartialEq)]
pub struct DayReport {
    pub day: NaiveDate,
    pub gates: Vec<Gate>,
    /// The day's own figures, for information: judged over the week.
    pub notes: Vec<String>,
    pub verdict: Verdict,
}

/// A store's week, judged.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub store: &'static str,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub basis: Basis,
    pub days: Vec<DayReport>,
    pub week: Vec<Gate>,
    /// Passing days in a row, up to the week's last.
    pub streak: usize,
    pub verdict: Verdict,
    /// The week's known-item runs, summed by group.
    pub known: Vec<KnownItemSummary>,
    /// Live comparisons of the whole week.
    pub live: WindowStats,
    /// Exit criteria this report cannot judge from Postgres.
    pub not_judged: Vec<&'static str>,
}

/// The exit criteria not judged here, and why: never counted as passed.
pub fn not_judged(store: &str) -> Vec<&'static str> {
    let mut out = vec![
        "a clean restore drill (A17): not recorded in Postgres",
        "the consistency checks (no data-loss findings): no shadow_checks table yet",
        "the kernel's busy and breaker_open drops, and a breaker open longer than a day: counted per hub process, in /api/metrics, not in Postgres",
    ];
    if store == "memories" {
        out.push("the mirror backlog reaching zero hourly, and lag p95 under 60 s: only the backlog now is known (/api/metrics, status), not its history");
    } else {
        out.push("\"Weaviate relevant, vellev none\" at most 2 %: needs the judge's grades of live searches, which are not kept");
    }
    out
}

/// Judge `input`'s week.
pub fn judge(input: &WeekInput) -> Report {
    let memories = input.store == "memories";
    // Artifacts' agreement is not judged on live searches (different
    // models): their volume gate stands alone.
    let basis = if !memories || input.week.answered >= MEMORY_WEEK_MINIMUM {
        Basis::Live
    } else if input.days.iter().any(|d| d.known.is_some()) {
        Basis::KnownItems
    } else {
        Basis::Nothing
    };
    let days: Vec<DayReport> = input
        .days
        .iter()
        .map(|day| {
            // Errors, empty answers and artifacts' recall are judged over
            // the week, where there are enough to: a day's are shown.
            let notes = vec![
                format!("errors and timeouts: {}", errors(&day.live).detail),
                format!(
                    "empty on vellev only: {}",
                    empty_on_vellev(&day.live).detail
                ),
            ];
            let mut gates = Vec::new();
            if memories {
                gates.push(memory_agreement(
                    basis,
                    &day.live,
                    day.known.as_deref(),
                    &Agreement {
                        gates: input.gates,
                        serving_alpha: input.serving_alpha,
                        self_first: None,
                        distinct: None,
                        day: true,
                    },
                ));
            }
            let verdict = worst(gates.iter().map(|g| g.verdict));
            DayReport {
                day: day.day,
                gates,
                notes,
                verdict,
            }
        })
        .collect();
    let known = sum_groups(input.days.iter().filter_map(|d| d.known.as_deref()));
    let any_known = input.days.iter().any(|d| d.known.is_some());
    let mut week = vec![
        volume(input.store, basis, &input.week),
        latency(&input.week),
        errors(&input.week),
        empty_on_vellev(&input.week),
    ];
    if memories {
        week.push(match alpha_changed(input) {
            Some(why) => gate("agreement (M2)", Verdict::TooFew, why),
            None => memory_agreement(
                basis,
                &input.week,
                any_known.then_some(known.as_slice()),
                &Agreement {
                    gates: input.gates,
                    serving_alpha: input.serving_alpha,
                    self_first: Some(input.self_first),
                    distinct: Some(&input.distinct),
                    day: false,
                },
            ),
        });
        week.push(reconcile_fixes(input.reconciles.as_ref()));
    } else {
        week.push(match alpha_changed(input) {
            Some(why) => gate("known-item recall@10", Verdict::TooFew, why),
            None => artifact_recall(any_known.then_some(known.as_slice()), input.serving_alpha),
        });
        week.push(artifact_judge(&known));
    }
    let streak = days
        .iter()
        .rev()
        // A day not judged (too few to) neither passes nor restarts.
        .take_while(|d| d.verdict <= Verdict::Pass)
        .count();
    let mut verdict = worst(
        days.iter()
            .map(|d| d.verdict)
            .chain(week.iter().map(|g| g.verdict)),
    );
    // Not judged decides nothing; all of it alone is no pass.
    if verdict == Verdict::NotJudged {
        verdict = Verdict::TooFew;
    }
    // Seven days are needed to pass; fewer (a window cut short) are too few.
    if verdict == Verdict::Pass && (days.len() as i64) < WEEK_DAYS {
        verdict = Verdict::TooFew;
    }
    Report {
        store: input.store,
        from: input.days.first().map(|d| d.day).unwrap_or_default(),
        to: input.days.last().map(|d| d.day).unwrap_or_default(),
        basis,
        days,
        week,
        streak,
        verdict,
        known,
        live: input.week.clone(),
        not_judged: not_judged(input.store),
    }
}

fn share(n: i64, of: i64) -> f64 {
    n as f64 / of as f64
}

/// The one-sided 95 % Wilson upper bound of `x` in `n`.
pub fn wilson_upper(x: i64, n: i64) -> f64 {
    let (x, n) = (x as f64, n as f64);
    let p = x / n;
    let z2 = Z95 * Z95;
    (p + z2 / (2.0 * n) + Z95 * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt()) / (1.0 + z2 / n)
}

/// A rate at most `most`, shown by its Wilson upper bound: pass when the
/// bound is within it; too few to judge when `n` is too small for even no
/// failure at all to show it; fail otherwise.
fn rate_gate(name: &'static str, x: i64, n: i64, most: f64, of: &str, extra: &str) -> Gate {
    if n == 0 {
        return gate(name, Verdict::TooFew, format!("no {of}"));
    }
    let upper = wilson_upper(x, n);
    let detail = format!(
        "{x} of {n} ({:.2} %; upper 95 % bound {:.2} %, at most {:.1} %){extra}",
        share(x, n) * 100.0,
        upper * 100.0,
        most * 100.0
    );
    let verdict = if upper <= most {
        Verdict::Pass
    } else if wilson_upper(0, n) > most {
        return gate(
            name,
            Verdict::TooFew,
            format!("{detail}: {n} cannot show it even with none"),
        );
    } else {
        Verdict::Fail
    };
    gate(name, verdict, detail)
}

/// Errors and timeouts at most [`MOST_ERRORS`] of the comparisons.
pub fn errors(live: &WindowStats) -> Gate {
    let bad = live.failed + live.panicked + live.timed_out;
    rate_gate(
        "errors and timeouts",
        bad,
        live.samples,
        MOST_ERRORS,
        "comparison",
        "",
    )
}

/// Answers empty on vellev alone at most [`MOST_EMPTY_ON_VELLEV`] of those
/// answered.
pub fn empty_on_vellev(live: &WindowStats) -> Gate {
    rate_gate(
        "empty on vellev only",
        live.empty_shadow_only,
        live.answered,
        MOST_EMPTY_ON_VELLEV,
        "answered comparison",
        &format!("; Weaviate only: {}", live.empty_primary_only),
    )
}

/// The week's minimum: of compared live recalls for memories (below it,
/// what the verdict rests on), of compared searches for artifacts.
pub fn volume(store: &str, basis: Basis, week: &WindowStats) -> Gate {
    let name = "volume";
    if store == "memories" {
        let detail = format!(
            "{} compared live recalls (minimum {MEMORY_WEEK_MINIMUM}): agreement {}",
            week.answered,
            basis.told()
        );
        let verdict = match basis {
            Basis::Live | Basis::KnownItems => Verdict::Pass,
            Basis::Nothing => Verdict::TooFew,
        };
        return gate(name, verdict, detail);
    }
    let verdict = if week.answered >= ARTIFACT_WEEK_MINIMUM {
        Verdict::Pass
    } else {
        Verdict::TooFew
    };
    gate(
        name,
        verdict,
        format!(
            "{} compared searches (minimum {ARTIFACT_WEEK_MINIMUM}, A14)",
            week.answered
        ),
    )
}

/// L1: vellev's p50 and p99 at or under Weaviate's, each over the answered
/// and the timed out (at their `shadow_ms`).
pub fn latency(live: &WindowStats) -> Gate {
    let name = "latency (L1)";
    let (p, s) = (&live.primary_wt, &live.shadow_wt);
    let (Some(p50), Some(p99), Some(s50), Some(s99)) = (p.p50, p.p99, s.p50, s.p99) else {
        return gate(name, Verdict::TooFew, "no answered or timed-out comparison");
    };
    let verdict = if s50 <= p50 && s99 <= p99 {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    gate(
        name,
        verdict,
        format!(
            "vellev p50 {s50:.1} ms, p99 {s99:.1} ms; Weaviate p50 {p50:.1} ms, p99 {p99:.1} ms ({} timed out counted at their time; answered only: vellev p99 {}, Weaviate p99 {})",
            live.timed_out,
            ms(live.shadow.p99),
            ms(live.primary.p99)
        ),
    )
}

fn ms(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.1} ms"))
        .unwrap_or_else(|| "-".into())
}

fn rate(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.3}")).unwrap_or_else(|| "-".into())
}

/// M2's thresholds on overlap@10, top-1 and RBO.
fn thresholds(
    gates: Option<AgreementGates>,
    overlap: Option<f64>,
    top1: Option<f64>,
    rbo: Option<f64>,
) -> (Verdict, String) {
    let got = format!(
        "overlap {}, top-1 {}, RBO {}",
        rate(overlap),
        rate(top1),
        rate(rbo)
    );
    let Some(g) = gates else {
        return (
            Verdict::NotSet,
            format!("{got}; [vellev.shadow.gates] not set"),
        );
    };
    let (Some(overlap), Some(top1), Some(rbo)) = (overlap, top1, rbo) else {
        return (Verdict::TooFew, got);
    };
    let verdict = if overlap >= g.overlap && top1 >= g.top1 && rbo >= g.rbo {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    (
        verdict,
        format!(
            "{got} (at least {:.3}, {:.3}, {:.3})",
            g.overlap, g.top1, g.rbo
        ),
    )
}

/// The paired non-inferiority test of self@1 at `margin`: `b` items found
/// first by Weaviate alone and `c` by vellev alone, of `n` pairs, judged
/// by the exact one-sided 95 % lower bound of the difference
/// ([`crate::shadow_stats::self_first_lower`]). Too few to judge when the
/// distinct items are under `min`, or when `n` pairs could not show the
/// margin even with no discordant pair.
pub fn self_first_test(
    g: &KnownItemSummary,
    margin: f64,
    min: usize,
    distinct: i64,
) -> (Verdict, String) {
    use crate::shadow_stats::{can_show, self_first_lower};
    let n = g.both_answered;
    let (b, c) = (g.only_primary_first, g.only_shadow_first);
    if distinct < min as i64 {
        return (
            Verdict::TooFew,
            format!("self@1 not judged: {distinct} distinct items both answered ({n} pairs), minimum {min}"),
        );
    }
    let as_u = |x: i64| u64::try_from(x).unwrap_or(0);
    if !can_show(as_u(n), margin) {
        return (
            Verdict::TooFew,
            format!("self@1 not judged: {n} pairs cannot show a margin of {:.2} points even with none discordant", margin * 100.0),
        );
    }
    let lower = self_first_lower(as_u(b), as_u(c), as_u(n));
    let verdict = if lower > -margin {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    (
        verdict,
        format!(
            "self@1 vellev {}/{n}, Weaviate {}/{n} ({distinct} distinct); b {b} c {c}: difference {:+.2} points, exact one-sided 95 % lower bound {:+.2} (margin -{:.2})",
            g.shadow_first,
            g.primary_first,
            (c - b) as f64 / n as f64 * 100.0,
            lower * 100.0,
            margin * 100.0
        ),
    )
}

/// Why the week's runs cannot be judged together: a run asked at another
/// serving alpha than the one configured (it changed mid-week).
pub fn alpha_changed(input: &WeekInput) -> Option<String> {
    let other: Vec<String> = input
        .days
        .iter()
        .filter_map(|d| d.run_alpha.map(|a| (d.day, a)))
        .filter(|(_, a)| (a - input.serving_alpha).abs() > 1e-6)
        .map(|(day, a)| format!("{day} at {a:.2}"))
        .collect();
    (!other.is_empty()).then(|| {
        format!(
            "the serving alpha changed mid-week: configured {:.2}, runs {}",
            input.serving_alpha,
            other.join(", ")
        )
    })
}

/// How agreement is judged: over a day, or over the week with self@1.
pub struct Agreement<'a> {
    pub gates: Option<AgreementGates>,
    pub serving_alpha: f32,
    pub self_first: Option<SelfFirstGate>,
    pub distinct: Option<&'a std::collections::BTreeMap<Population, i64>>,
    /// A day's: below [`DAY_M2_MINIMUM`] not judged.
    pub day: bool,
}

fn is_gating(g: &KnownItemSummary, serving: f32) -> bool {
    g.mode == QueryMode::Window && (g.alpha - serving).abs() <= 1e-6
}

/// Memories' agreement (M2), on what the week rests on: the live recalls'
/// means, or the known-item runs.
///
/// On the runs, two groups gate: scoped memories and the unscoped
/// population, each in window mode at the serving alpha. Each meets M2's
/// thresholds and, with `self_first` (over the week), the self@1 test at
/// its own minimum size. The hard mode and alpha 1.0 are reported, not
/// judged.
pub fn memory_agreement(
    basis: Basis,
    live: &WindowStats,
    known: Option<&[KnownItemSummary]>,
    how: &Agreement,
) -> Gate {
    let gates = how.gates;
    let self_first = how.self_first;
    let thin = |n: i64| how.day && n < DAY_M2_MINIMUM;
    let name = "agreement (M2)";
    match basis {
        Basis::Nothing => gate(
            name,
            Verdict::TooFew,
            "too few live recalls and no known-item run",
        ),
        Basis::Live => {
            if live.answered == 0 {
                return gate(name, Verdict::TooFew, "no answered recall");
            }
            if thin(live.answered) {
                let (_, detail) =
                    thresholds(gates, live.mean_overlap, live.top1_rate, live.mean_rbo);
                return gate(
                    name,
                    Verdict::NotJudged,
                    format!(
                        "{} recalls, under the day's {DAY_M2_MINIMUM}: live {detail}",
                        live.answered
                    ),
                );
            }
            let (verdict, detail) =
                thresholds(gates, live.mean_overlap, live.top1_rate, live.mean_rbo);
            gate(name, verdict, format!("live: {detail}"))
        }
        Basis::KnownItems => {
            let Some(groups) = known.filter(|g| !g.is_empty()) else {
                return gate(
                    name,
                    Verdict::TooFew,
                    "no known-item run that ran to its end",
                );
            };
            let serving = how.serving_alpha;
            let mut verdicts = Vec::new();
            let mut details = Vec::new();
            for population in [Population::Scoped, Population::Unscoped] {
                let found = groups
                    .iter()
                    .find(|g| g.population == population && is_gating(g, serving));
                let Some(g) = found else {
                    verdicts.push(if how.day {
                        Verdict::NotJudged
                    } else {
                        Verdict::TooFew
                    });
                    details.push(format!(
                        "{} window alpha {serving:.2}: no items",
                        population.as_str()
                    ));
                    continue;
                };
                let label = group_label(g);
                if g.both_answered == 0 {
                    verdicts.push(Verdict::TooFew);
                    details.push(format!("{label}: no item both answered"));
                    continue;
                }
                let (agreed, detail) = thresholds(gates, g.mean_overlap, g.top1_rate, g.mean_rbo);
                if thin(g.both_answered) {
                    verdicts.push(Verdict::NotJudged);
                    details.push(format!(
                        "{label}: {} both answered, under the day's {DAY_M2_MINIMUM}: {detail}",
                        g.both_answered
                    ));
                    continue;
                }
                let mut verdict = agreed;
                let mut said = format!("{label}: {detail}");
                if let Some(test) = self_first {
                    let min = match population {
                        Population::Scoped => test.min,
                        Population::Unscoped => test.unscoped_min,
                    };
                    let distinct = how
                        .distinct
                        .and_then(|d| d.get(&population).copied())
                        .unwrap_or(g.both_answered);
                    let (first, told) = self_first_test(g, test.margin, min, distinct);
                    verdict = verdict.max(first);
                    said.push_str(&format!("; {told}"));
                }
                said.push_str(&format!(
                    " (found by a duplicate within k: vellev {}, Weaviate {})",
                    g.shadow_found_by_duplicate, g.primary_found_by_duplicate
                ));
                verdicts.push(verdict);
                details.push(said);
            }
            for g in groups.iter().filter(|g| !is_gating(g, serving)) {
                details.push(format!(
                    "{} (reported, not judged): overlap {}, top-1 {}, RBO {}; self@1 vellev {}/{n}, Weaviate {}/{n}, b {} c {}",
                    group_label(g),
                    rate(g.mean_overlap),
                    rate(g.top1_rate),
                    rate(g.mean_rbo),
                    g.shadow_first,
                    g.primary_first,
                    g.only_primary_first,
                    g.only_shadow_first,
                    n = g.both_answered
                ));
            }
            gate(name, worst(verdicts), details.join("; "))
        }
    }
}

/// A known-item group as a person reads it: the unscoped memories (filed
/// under no identity) are their own population.
pub fn group_label(g: &KnownItemSummary) -> String {
    format!(
        "{} {} alpha {:.2}",
        g.population.as_str(),
        g.mode.as_str(),
        g.alpha
    )
}

/// Artifacts' known-item recall@10: vellev's at least Weaviate's less
/// [`ARTIFACT_RECALL_SLACK`], on the window mode at the serving alpha over
/// at least [`ARTIFACT_KNOWN_MINIMUM`] items; any other group reported.
pub fn artifact_recall(known: Option<&[KnownItemSummary]>, serving: f32) -> Gate {
    let name = "known-item recall@10";
    let Some(groups) = known.filter(|g| !g.is_empty()) else {
        return gate(
            name,
            Verdict::TooFew,
            "no known-item run that ran to its end",
        );
    };
    let told = |g: &KnownItemSummary| {
        let p = share(g.primary_within_k, g.both_answered.max(1));
        let s = share(g.shadow_within_k, g.both_answered.max(1));
        format!(
            "{}: vellev {s:.3}, Weaviate {p:.3} over {} (recall@5 {} vs {}; MRR {} vs {})",
            group_label(g),
            g.both_answered,
            g.shadow_within_5,
            g.primary_within_5,
            rate(g.shadow_mrr),
            rate(g.primary_mrr)
        )
    };
    let mut details: Vec<String> = groups
        .iter()
        .filter(|g| !is_gating(g, serving))
        .map(|g| format!("{} (reported, not judged)", told(g)))
        .collect();
    let Some(g) = groups.iter().find(|g| is_gating(g, serving)) else {
        details.insert(0, format!("no window items at alpha {serving:.2}"));
        return gate(name, Verdict::TooFew, details.join("; "));
    };
    if g.both_answered < ARTIFACT_KNOWN_MINIMUM {
        details.insert(
            0,
            format!(
                "{}; minimum {ARTIFACT_KNOWN_MINIMUM} both answered",
                told(g)
            ),
        );
        return gate(name, Verdict::TooFew, details.join("; "));
    }
    let p = share(g.primary_within_k, g.both_answered);
    let s = share(g.shadow_within_k, g.both_answered);
    let verdict = if s >= p - ARTIFACT_RECALL_SLACK {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    details.insert(0, told(g));
    gate(name, verdict, details.join("; "))
}

/// The judge's nDCG@5, a gate only once the week has
/// [`JUDGED_WEEK_MINIMUM`] judged queries; with the judge off, nothing.
pub fn artifact_judge(known: &[KnownItemSummary]) -> Gate {
    let name = "judged nDCG@5";
    let judged: i64 = known.iter().map(|g| g.judged).sum();
    if judged == 0 {
        return gate(name, Verdict::NotJudged, "the judge is off: nothing judged");
    }
    if judged < JUDGED_WEEK_MINIMUM {
        return gate(
            name,
            Verdict::TooFew,
            format!("{judged} judged (minimum {JUDGED_WEEK_MINIMUM})"),
        );
    }
    let mean = |side: fn(&KnownItemSummary) -> Option<f64>| {
        let (sum, n) = known
            .iter()
            .filter_map(|g| side(g).map(|v| (v * g.judged as f64, g.judged)))
            .fold((0.0, 0), |(s, n), (v, j)| (s + v, n + j));
        (n > 0).then(|| sum / n as f64)
    };
    let (Some(p), Some(s)) = (mean(|g| g.primary_ndcg), mean(|g| g.shadow_ndcg)) else {
        return gate(name, Verdict::TooFew, "no nDCG recorded");
    };
    let verdict = if s >= p - ARTIFACT_NDCG_SLACK {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    gate(
        name,
        verdict,
        format!("vellev {s:.3}, Weaviate {p:.3} over {judged} judged"),
    )
}

/// R1: no unexplained fix in the last [`R1_DAYS`] days. A fix is
/// unexplained until a cause is recorded for it: while the reconcile
/// records none, R1 is not judged, and the fixes are shown.
pub fn reconcile_fixes(reconciles: Option<&Reconciles>) -> Gate {
    let name = "reconcile fixes (R1)";
    let Some(r) = reconciles.filter(|r| r.ok_runs > 0) else {
        return gate(
            name,
            Verdict::TooFew,
            format!("no reconcile ran to its end in the last {R1_DAYS} days"),
        );
    };
    let fixes = r.fixes.len() as i64 + r.fixes_left_out;
    if !r.causes_recorded {
        return gate(
            name,
            Verdict::NotJudged,
            format!(
                "{fixes} fixes over {} reconciles in the last {R1_DAYS} days; no cause is recorded for a fix yet, so each is unexplained until one is, and R1 is not judged",
                r.ok_runs
            ),
        );
    }
    let explained = r.fixes.iter().filter(|f| f.explained.is_some()).count() as i64;
    let unexplained = fixes - explained;
    let verdict = if unexplained == 0 {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    gate(
        name,
        verdict,
        format!(
            "{unexplained} unexplained, {explained} explained, over {} reconciles in the last {R1_DAYS} days",
            r.ok_runs
        ),
    )
}

/// Known-item groups summed over runs: counts added, means weighted by the
/// items both sides answered.
pub fn sum_groups<'a>(
    runs: impl IntoIterator<Item = &'a [KnownItemSummary]>,
) -> Vec<KnownItemSummary> {
    type Key = (Population, QueryMode, u32);
    let mut sums: std::collections::BTreeMap<Key, (KnownItemSummary, [f64; 8], [i64; 8])> =
        std::collections::BTreeMap::new();
    for run in runs {
        for g in run {
            let key = (g.population, g.mode, g.alpha.to_bits());
            let (s, means, weights) = sums.entry(key).or_insert_with(|| {
                (
                    KnownItemSummary {
                        population: g.population,
                        mode: g.mode,
                        alpha: g.alpha,
                        ..KnownItemSummary::default()
                    },
                    [0.0; 8],
                    [0; 8],
                )
            });
            s.items += g.items;
            s.asked += g.asked;
            s.embed_failed += g.embed_failed;
            s.no_permit += g.no_permit;
            s.primary_errors += g.primary_errors;
            s.shadow_errors += g.shadow_errors;
            s.both_answered += g.both_answered;
            s.primary_first += g.primary_first;
            s.shadow_first += g.shadow_first;
            s.only_primary_first += g.only_primary_first;
            s.only_shadow_first += g.only_shadow_first;
            s.primary_within_k += g.primary_within_k;
            s.shadow_within_k += g.shadow_within_k;
            s.primary_within_5 += g.primary_within_5;
            s.shadow_within_5 += g.shadow_within_5;
            s.judged += g.judged;
            s.primary_found_by_duplicate += g.primary_found_by_duplicate;
            s.shadow_found_by_duplicate += g.shadow_found_by_duplicate;
            let values = [
                (g.primary_mrr, g.both_answered),
                (g.shadow_mrr, g.both_answered),
                (g.mean_overlap, g.both_answered),
                (g.mean_rbo, g.both_answered),
                (g.mean_jaccard, g.both_answered),
                (g.top1_rate, g.both_answered),
                (g.primary_ndcg, g.judged),
                (g.shadow_ndcg, g.judged),
            ];
            for (i, (v, w)) in values.into_iter().enumerate() {
                if let Some(v) = v {
                    means[i] += v * w as f64;
                    weights[i] += w;
                }
            }
        }
    }
    sums.into_values()
        .map(|(mut s, means, weights)| {
            let mean = |i: usize| (weights[i] > 0).then(|| means[i] / weights[i] as f64);
            s.primary_mrr = mean(0);
            s.shadow_mrr = mean(1);
            s.mean_overlap = mean(2);
            s.mean_rbo = mean(3);
            s.mean_jaccard = mean(4);
            s.top1_rate = mean(5);
            s.primary_ndcg = mean(6);
            s.shadow_ndcg = mean(7);
            s
        })
        .collect()
}

/// The ISO week (Monday to Sunday, UTC) that `day` falls in: its first day.
pub fn week_of(day: NaiveDate) -> NaiveDate {
    day - Days::days(i64::from(day.weekday().num_days_from_monday()))
}

/// Read what `store`'s seven days from `from` are judged on, from
/// Postgres alone.
pub async fn gather(
    tables: &ShadowStore,
    config: &WorkforceConfig,
    store: &'static str,
    from: NaiveDate,
) -> Result<WeekInput> {
    let to = from + Days::days(WEEK_DAYS);
    let mut days = Vec::new();
    let mut runs = Vec::new();
    for offset in 0..WEEK_DAYS {
        let day = from + Days::days(offset);
        let live = tables
            .window_stats(store, day_start(day), day_start(day + Days::days(1)))
            .await?;
        let (known, run_alpha) = match tables.counted_known_item_run(store, day).await? {
            Some((run, alpha)) => {
                runs.push(run);
                (Some(tables.known_item_summary(run).await?), Some(alpha))
            }
            None => (None, None),
        };
        days.push(Day {
            day,
            live,
            known,
            run_alpha,
        });
    }
    // The week's percentiles from its raw rows, never from the days'.
    let week = tables
        .window_stats(store, day_start(from), day_start(to))
        .await?;
    // Memories are asked at the hub's own alpha; artifacts at 1.0 alone.
    let serving_alpha = if store == "memories" {
        config.embeddings.alpha.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let mut distinct = std::collections::BTreeMap::new();
    if !runs.is_empty() {
        for population in [Population::Scoped, Population::Unscoped] {
            let n = tables
                .distinct_known_items(
                    &runs,
                    population.as_str(),
                    QueryMode::Window.as_str(),
                    serving_alpha,
                )
                .await?;
            distinct.insert(population, n);
        }
    }
    let reconciles = if store == "memories" {
        let window = tables
            .reconcile_window(store, day_start(to - Days::days(R1_DAYS)), day_start(to))
            .await?;
        Some(Reconciles {
            ok_runs: window.ok_runs,
            // Nothing in Postgres records what explains a fix yet.
            fixes: window
                .fixes
                .into_iter()
                .map(|f| Fix {
                    at: f.at,
                    memory_id: f.memory_id,
                    explained: None,
                })
                .collect(),
            fixes_left_out: window.fixes_left_out,
            // The reconcile records what it fixed, not why.
            causes_recorded: false,
        })
    } else {
        None
    };
    Ok(WeekInput {
        store,
        days,
        week,
        gates: config.vellev.shadow.gates()?,
        reconciles,
        self_first: config.vellev.shadow.self_first_gate()?,
        serving_alpha,
        distinct,
    })
}

/// The stores a report may be asked of.
pub fn store_named(name: &str) -> Option<&'static str> {
    match name {
        "memories" => Some("memories"),
        "artifacts" => Some("artifacts"),
        _ => None,
    }
}

/// Record the verdict of every whole ISO week of every store that holds
/// comparisons before `today` and has none recorded yet. Returns how many
/// were recorded. Run by the leader's retention pass before it deletes any
/// comparison ([`verdicts_first`]).
pub async fn record_due_verdicts(
    tables: &ShadowStore,
    config: &WorkforceConfig,
    today: NaiveDate,
) -> Result<usize> {
    let mut weeks = std::collections::BTreeSet::new();
    for (store, day) in tables.compared_days(day_start(today)).await? {
        let Some(store) = store_named(&store) else {
            continue;
        };
        let start = week_of(day);
        // Whole weeks only: one still running is judged once it is over.
        if start + Days::days(WEEK_DAYS) <= today {
            weeks.insert((store, start));
        }
    }
    let recorded: std::collections::BTreeSet<(String, NaiveDate)> = tables
        .verdicts(None, i64::MAX)
        .await?
        .into_iter()
        .map(|v| (v.store, v.week_start))
        .collect();
    let mut n = 0;
    for (store, start) in weeks {
        if recorded.contains(&(store.to_string(), start)) {
            continue;
        }
        let report = judge(&gather(tables, config, store, start).await?);
        tables
            .record_verdict(
                store,
                report.from,
                report.to,
                report.verdict.as_str(),
                &to_json(&report),
            )
            .await
            .with_context(|| format!("recording {store}'s verdict of the week from {start}"))?;
        n += 1;
    }
    Ok(n)
}

/// The comparisons' retention pass, verdicts first: `prune` runs only once
/// `record` has recorded every due week's verdict. If recording fails, the
/// comparisons are left for the next pass, never deleted unjudged.
pub fn verdicts_first(
    record: impl FnOnce() -> Result<usize>,
    prune: impl FnOnce() -> Result<usize>,
) -> Result<usize> {
    record().context("the comparisons are kept until their weeks' verdicts are recorded")?;
    prune()
}

fn window_json(w: &WindowStats) -> serde_json::Value {
    let p = |p: &workforce_common::shadow_postgres::Percentiles| json!({"p50": p.p50, "p95": p.p95, "p99": p.p99});
    json!({
        "samples": w.samples, "answered": w.answered, "failed": w.failed,
        "panicked": w.panicked, "timed_out": w.timed_out, "probes": w.probes,
        "mean_overlap": w.mean_overlap, "mean_rbo": w.mean_rbo,
        "mean_jaccard": w.mean_jaccard, "top1_rate": w.top1_rate,
        "empty_primary_only": w.empty_primary_only,
        "empty_shadow_only": w.empty_shadow_only,
        "primary_ms": p(&w.primary), "shadow_ms": p(&w.shadow),
        "primary_ms_with_timeouts": p(&w.primary_wt),
        "shadow_ms_with_timeouts": p(&w.shadow_wt),
        "shadow_parts": parts_json(&w.parts),
    })
}

fn parts_json(b: &Breakdown) -> serde_json::Value {
    json!({
        "labelled": b.labelled,
        "queue_ms": {"p50": b.queue_p50, "p99": b.queue_p99},
        "index_ms": {"p50": b.index_p50, "p99": b.index_p99},
        "alone": {"n": b.alone, "p50": b.alone_p50, "p99": b.alone_p99},
        "in_a_burst": {"n": b.burst, "p50": b.burst_p50, "p99": b.burst_p99},
    })
}

/// Where vellev's time went, as a line of the report: `None` before the
/// rows carry PR 9b's labels.
pub fn parts_text(b: &Breakdown) -> Option<String> {
    (b.labelled > 0).then(|| {
        format!(
            "vellev's time in parts, over {} labelled: queued p50 {} p99 {} (outside its time); index call p50 {} p99 {}; alone ({}) p50 {} p99 {}; in a burst ({}) p50 {} p99 {}",
            b.labelled,
            ms(b.queue_p50),
            ms(b.queue_p99),
            ms(b.index_p50),
            ms(b.index_p99),
            b.alone,
            ms(b.alone_p50),
            ms(b.alone_p99),
            b.burst,
            ms(b.burst_p50),
            ms(b.burst_p99)
        )
    })
}

fn gate_json(g: &Gate) -> serde_json::Value {
    json!({"gate": g.name, "verdict": g.verdict.as_str(), "told": g.verdict.told(), "detail": g.detail})
}

fn known_json(g: &KnownItemSummary) -> serde_json::Value {
    json!({
        "population": g.population.as_str(), "mode": g.mode.as_str(), "alpha": g.alpha,
        "items": g.items, "both_answered": g.both_answered,
        "primary_first": g.primary_first, "shadow_first": g.shadow_first,
        "only_primary_first": g.only_primary_first, "only_shadow_first": g.only_shadow_first,
        "primary_within_k": g.primary_within_k, "shadow_within_k": g.shadow_within_k,
        "primary_found_by_duplicate": g.primary_found_by_duplicate,
        "shadow_found_by_duplicate": g.shadow_found_by_duplicate,
        "primary_mrr": g.primary_mrr, "shadow_mrr": g.shadow_mrr,
        "mean_overlap": g.mean_overlap, "mean_rbo": g.mean_rbo, "top1_rate": g.top1_rate,
        "embed_failed": g.embed_failed, "no_permit": g.no_permit,
        "primary_errors": g.primary_errors, "shadow_errors": g.shadow_errors,
        "judged": g.judged, "primary_ndcg": g.primary_ndcg, "shadow_ndcg": g.shadow_ndcg,
    })
}

/// The report as the admin route answers and `shadow_verdicts` keeps it.
pub fn to_json(r: &Report) -> serde_json::Value {
    json!({
        "store": r.store,
        "from": r.from,
        "to": r.to,
        "verdict": r.verdict.as_str(),
        "told": r.verdict.told(),
        "basis": r.basis.told(),
        "streak": r.streak,
        "week": r.week.iter().map(gate_json).collect::<Vec<_>>(),
        "days": r.days.iter().map(|d| json!({
            "day": d.day, "verdict": d.verdict.as_str(),
            "gates": d.gates.iter().map(gate_json).collect::<Vec<_>>(),
            "information": d.notes,
        })).collect::<Vec<_>>(),
        "live": window_json(&r.live),
        "known_items": r.known.iter().map(known_json).collect::<Vec<_>>(),
        "not_judged": r.not_judged,
    })
}

/// The report as `workforce shadow report` prints it.
pub fn to_text(r: &Report) -> String {
    let mut out = format!(
        "{} {} to {}: {} (passing days in a row: {}; agreement {})\n",
        r.store,
        r.from,
        r.to,
        r.verdict.told().to_uppercase(),
        r.streak,
        r.basis.told()
    );
    out.push_str("  the week:\n");
    for g in &r.week {
        out.push_str(&format!(
            "    {:<24} {:<20} {}\n",
            g.name,
            g.verdict.told(),
            g.detail
        ));
    }
    for d in &r.days {
        out.push_str(&format!("  {}: {}\n", d.day, d.verdict.told()));
        for g in &d.gates {
            out.push_str(&format!(
                "    {:<24} {:<20} {}\n",
                g.name,
                g.verdict.told(),
                g.detail
            ));
        }
        for n in &d.notes {
            out.push_str(&format!("    (information) {n}\n"));
        }
    }
    let w = &r.live;
    out.push_str(&format!(
        "  live: {} compared ({} answered, {} failed, {} panicked, {} timed out, {} probes); p50/p95/p99 vellev {}/{}/{} Weaviate {}/{}/{} (answered only)\n",
        w.samples, w.answered, w.failed, w.panicked, w.timed_out, w.probes,
        ms(w.shadow.p50), ms(w.shadow.p95), ms(w.shadow.p99),
        ms(w.primary.p50), ms(w.primary.p95), ms(w.primary.p99),
    ));
    out.push_str(&format!(
        "  {}\n",
        parts_text(&w.parts).unwrap_or_else(|| {
            "vellev's time in parts: not recorded (rows without PR 9b's labels)".into()
        })
    ));
    for g in &r.known {
        out.push_str(&format!(
            "  known items, {}: {} items, {} both answered; self@1 vellev {} Weaviate {} (found by a duplicate within k: vellev {}, Weaviate {}); {} embed failed, {} no permit, errors vellev {} Weaviate {}\n",
            group_label(g), g.items, g.both_answered, g.shadow_first, g.primary_first,
            g.shadow_found_by_duplicate, g.primary_found_by_duplicate,
            g.embed_failed, g.no_permit, g.shadow_errors, g.primary_errors
        ));
    }
    out.push_str("  not judged here (never counted as passed):\n");
    for n in &r.not_judged {
        out.push_str(&format!("    - {n}\n"));
    }
    out
}

#[cfg(test)]
#[path = "shadow_report_tests.rs"]
mod tests;
