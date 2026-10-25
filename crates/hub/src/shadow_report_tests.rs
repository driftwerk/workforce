//! The report's rules (DW-676, PR 9c), each on its own, then the whole
//! read from a real Postgres.

use super::*;
use chrono::TimeZone;
use workforce_common::shadow_postgres::Percentiles;

fn day(n: i64) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 12).unwrap() + Days::days(n)
}

fn gates() -> Option<AgreementGates> {
    Some(AgreementGates {
        overlap: 0.9,
        top1: 0.9,
        rbo: 0.85,
    })
}

/// `answered` comparisons, all agreeing, vellev faster than Weaviate.
fn live(answered: i64) -> WindowStats {
    let fast = Percentiles {
        p50: Some(5.0),
        p95: Some(8.0),
        p99: Some(9.0),
    };
    let slow = Percentiles {
        p50: Some(10.0),
        p95: Some(15.0),
        p99: Some(20.0),
    };
    WindowStats {
        samples: answered,
        answered,
        mean_overlap: Some(0.97),
        mean_rbo: Some(0.95),
        top1_rate: Some(0.98),
        primary: slow,
        shadow: fast,
        primary_wt: slow,
        shadow_wt: fast,
        ..WindowStats::default()
    }
}

/// A known-item group of `n` items both answered, vellev and Weaviate
/// finding all first and agreeing.
fn known(population: Population, n: i64) -> KnownItemSummary {
    KnownItemSummary {
        population,
        mode: QueryMode::Window,
        alpha: 0.75,
        items: n,
        asked: n,
        both_answered: n,
        primary_first: n,
        shadow_first: n,
        primary_within_k: n,
        shadow_within_k: n,
        mean_overlap: Some(0.97),
        mean_rbo: Some(0.95),
        top1_rate: Some(0.99),
        ..KnownItemSummary::default()
    }
}

fn reconciles() -> Option<Reconciles> {
    Some(Reconciles {
        ok_runs: 288,
        ..Reconciles::default()
    })
}

/// A week of memories: each day's live recalls, and each day's run.
fn memory_week(per_day: i64, runs: bool) -> WeekInput {
    let days: Vec<Day> = (0..7)
        .map(|n| Day {
            day: day(n),
            live: live(per_day),
            known: runs.then(|| {
                vec![
                    known(Population::Scoped, 1000),
                    known(Population::Unscoped, 250),
                ]
            }),
            run_alpha: runs.then_some(0.75),
        })
        .collect();
    WeekInput {
        store: "memories",
        days,
        week: live(per_day * 7),
        gates: gates(),
        reconciles: reconciles(),
        self_first: SelfFirstGate::default(),
        serving_alpha: 0.75,
        distinct: Default::default(),
    }
}

fn artifact_week(per_day: i64) -> WeekInput {
    WeekInput {
        store: "artifacts",
        days: (0..7)
            .map(|n| Day {
                day: day(n),
                live: live(per_day),
                known: Some(vec![KnownItemSummary {
                    alpha: 1.0,
                    ..known(Population::Scoped, 100)
                }]),
                run_alpha: Some(1.0),
            })
            .collect(),
        week: live(per_day * 7),
        gates: None,
        reconciles: None,
        self_first: SelfFirstGate::default(),
        serving_alpha: 1.0,
        distinct: Default::default(),
    }
}

fn week_gate<'a>(report: &'a Report, name: &str) -> &'a Gate {
    report
        .week
        .iter()
        .find(|g| g.name == name)
        .unwrap_or_else(|| panic!("no gate {name}: {report:#?}"))
}

/// The fixtures pass as they are, so each test below moves one thing.
#[test]
fn a_good_week_passes() {
    let report = judge(&memory_week(200, false));
    assert_eq!(report.verdict, Verdict::Pass, "{report:#?}");
    assert_eq!(report.basis, Basis::Live);
    assert_eq!(report.streak, 7);
    let report = judge(&artifact_week(100));
    assert_eq!(report.verdict, Verdict::Pass, "{report:#?}");
}

/// Rule 1: an empty week, or a store with no sample, is too few to judge,
/// gate by gate, never a pass.
#[test]
fn an_empty_week_is_too_few_to_judge_at_every_gate() {
    for input in [
        WeekInput {
            store: "memories",
            days: (0..7)
                .map(|n| Day {
                    day: day(n),
                    ..Day::default()
                })
                .collect(),
            week: WindowStats::default(),
            gates: gates(),
            reconciles: None,
            self_first: SelfFirstGate::default(),
            serving_alpha: 0.75,
            distinct: Default::default(),
        },
        WeekInput {
            store: "artifacts",
            days: (0..7)
                .map(|n| Day {
                    day: day(n),
                    ..Day::default()
                })
                .collect(),
            week: WindowStats::default(),
            gates: None,
            reconciles: None,
            self_first: SelfFirstGate::default(),
            serving_alpha: 0.75,
            distinct: Default::default(),
        },
    ] {
        let report = judge(&input);
        assert_eq!(report.verdict, Verdict::TooFew, "{report:#?}");
        for g in &report.week {
            // The judge's gate is no gate with the judge off.
            if g.name != "judged nDCG@5" {
                assert_eq!(g.verdict, Verdict::TooFew, "{}: {g:?}", input.store);
            }
        }
        for d in &report.days {
            for g in &d.gates {
                assert_eq!(g.verdict, Verdict::TooFew, "{}: {g:?}", input.store);
            }
        }
    }
    // One day with no answered sample: that day's gates are too few, and
    // so is the week, though every other day passes.
    let mut input = memory_week(200, false);
    input.days[3].live = WindowStats::default();
    let report = judge(&input);
    assert_eq!(report.days[3].verdict, Verdict::TooFew, "{report:#?}");
    assert_eq!(report.verdict, Verdict::TooFew);
    // No reconcile in the last three days: R1 is too few, not a pass.
    let mut input = memory_week(200, false);
    input.reconciles = Some(Reconciles::default());
    let report = judge(&input);
    assert_eq!(
        week_gate(&report, "reconcile fixes (R1)").verdict,
        Verdict::TooFew
    );
    assert_eq!(report.verdict, Verdict::TooFew);
}

/// Rule 2: 999 live recalls and no known-item run is too few to judge; with
/// runs, the verdict says it rests on them, and is judged on them.
#[test]
fn memories_under_the_minimum_rest_on_the_known_item_runs_or_are_too_few() {
    let mut input = memory_week(0, false);
    input.week = live(999);
    for d in &mut input.days {
        d.live = live(143);
    }
    let report = judge(&input);
    assert_eq!(report.basis, Basis::Nothing);
    assert_eq!(report.verdict, Verdict::TooFew, "{report:#?}");
    assert_eq!(week_gate(&report, "volume").verdict, Verdict::TooFew);

    let mut input = memory_week(143, true);
    input.week = live(999);
    let report = judge(&input);
    assert_eq!(report.basis, Basis::KnownItems);
    assert!(
        week_gate(&report, "volume")
            .detail
            .contains("rests on known-item runs"),
        "{report:#?}"
    );
    assert_eq!(report.verdict, Verdict::Pass, "{report:#?}");
    // Judged on the runs: vellev finding clearly fewer first than Weaviate
    // fails, though every live recall agreed.
    {
        let g = &mut input.days[6].known.as_mut().unwrap()[1];
        g.only_primary_first = 30;
        g.shadow_first -= 30;
    }
    let report = judge(&input);
    assert_eq!(report.verdict, Verdict::Fail, "{report:#?}");
    // Self@1 is judged over the week, summed, not day by day.
    assert_eq!(week_gate(&report, "agreement (M2)").verdict, Verdict::Fail);
    assert_eq!(report.days[6].verdict, Verdict::Pass, "{report:#?}");
    // The unscoped memories are their own population, told apart.
    assert!(
        week_gate(&report, "agreement (M2)")
            .detail
            .contains("unscoped window"),
        "{report:#?}"
    );
    // At 1,000 the live recalls carry it, the runs aside.
    input.week = live(1000);
    let report = judge(&input);
    assert_eq!(report.basis, Basis::Live);
}

/// Rule 3: 199 compared artifact searches are too few to judge (A14).
#[test]
fn artifacts_under_200_searches_are_too_few_to_judge() {
    let mut input = artifact_week(100);
    input.week = live(199);
    let report = judge(&input);
    assert_eq!(week_gate(&report, "volume").verdict, Verdict::TooFew);
    assert_eq!(report.verdict, Verdict::TooFew, "{report:#?}");
    input.week = live(200);
    assert_eq!(week_gate(&judge(&input), "volume").verdict, Verdict::Pass);
}

/// Rule 5: L1 is judged on the percentiles that count the timed out; the
/// answered-only ones, which leave vellev's slowest out, never decide it.
#[test]
fn latency_is_judged_with_the_timeouts_counted() {
    let mut input = memory_week(200, false);
    // Answered only, vellev is faster; with its one timeout at the
    // deadline, its p99 is past Weaviate's.
    input.week.timed_out = 1;
    input.week.samples += 1;
    input.week.shadow_wt.p99 = Some(2000.0);
    let report = judge(&input);
    assert_eq!(
        week_gate(&report, "latency (L1)").verdict,
        Verdict::Fail,
        "{report:#?}"
    );
    // And the other way: slow answered-only figures do not fail it.
    let mut input = memory_week(200, false);
    input.week.shadow.p99 = Some(500.0);
    let report = judge(&input);
    assert_eq!(week_gate(&report, "latency (L1)").verdict, Verdict::Pass);
    // p50 counts as well as p99.
    let mut input = memory_week(200, false);
    input.week.shadow_wt.p50 = Some(10.5);
    assert_eq!(
        week_gate(&judge(&input), "latency (L1)").verdict,
        Verdict::Fail
    );
}

/// Rule 6: with no M2 thresholds, agreement says "thresholds not set",
/// never pass, on either basis.
#[test]
fn agreement_without_thresholds_is_not_set() {
    for runs in [false, true] {
        let mut input = memory_week(if runs { 10 } else { 200 }, runs);
        input.gates = None;
        let report = judge(&input);
        assert_eq!(
            week_gate(&report, "agreement (M2)").verdict,
            Verdict::NotSet,
            "{report:#?}"
        );
        assert_eq!(report.verdict, Verdict::NotSet);
        assert_eq!(report.verdict.told(), "thresholds not set");
    }
    // Below a threshold fails.
    let mut input = memory_week(200, false);
    input.week.mean_rbo = Some(0.84);
    assert_eq!(judge(&input).verdict, Verdict::Fail);
}

/// Rule 7: a fix is unexplained until a cause is recorded for it. While
/// the reconcile records none, R1 is not judged (never a pass, deciding
/// nothing) and the fixes are shown; once causes are recorded, an
/// unexplained fix in the last three days fails it, an explained one does
/// not, and a fix only counted, never named, is unexplained.
#[test]
fn r1_is_not_judged_until_causes_are_recorded() {
    let at = Utc.with_ymd_and_hms(2026, 10, 17, 3, 0, 0).unwrap();
    let fix = |explained: Option<&str>| Fix {
        at,
        memory_id: Some("m1".into()),
        explained: explained.map(str::to_string),
    };
    let mut input = memory_week(200, false);
    input.reconciles = Some(Reconciles {
        ok_runs: 10,
        fixes: vec![fix(None), fix(None)],
        ..Reconciles::default()
    });
    let report = judge(&input);
    let r1 = week_gate(&report, "reconcile fixes (R1)");
    assert_eq!(r1.verdict, Verdict::NotJudged, "{report:#?}");
    assert!(
        r1.detail.contains("2 fixes") && r1.detail.contains("unexplained until"),
        "{r1:?}"
    );
    assert_eq!(report.verdict, Verdict::Pass, "not judged decides nothing");

    input.reconciles = Some(Reconciles {
        ok_runs: 10,
        fixes: vec![fix(Some("a CLI write"))],
        fixes_left_out: 0,
        causes_recorded: true,
    });
    assert_eq!(judge(&input).verdict, Verdict::Pass);
    input.reconciles.as_mut().unwrap().fixes.push(fix(None));
    let report = judge(&input);
    assert_eq!(
        week_gate(&report, "reconcile fixes (R1)").verdict,
        Verdict::Fail
    );
    assert_eq!(report.verdict, Verdict::Fail);
    input.reconciles = Some(Reconciles {
        ok_runs: 10,
        fixes: vec![],
        fixes_left_out: 1,
        causes_recorded: true,
    });
    assert_eq!(judge(&input).verdict, Verdict::Fail);
}

/// Rule 8: a day that fails a gate restarts the count: the week fails,
/// and the passing days are counted from the day after it.
#[test]
fn a_failing_day_restarts_the_count() {
    let mut input = memory_week(200, false);
    input.days[2].live.mean_rbo = Some(0.5);
    let report = judge(&input);
    assert_eq!(report.days[2].verdict, Verdict::Fail);
    assert_eq!(report.streak, 4);
    assert_eq!(report.verdict, Verdict::Fail, "{report:#?}");
    // A known-item day failing does the same.
    let mut input = memory_week(10, true);
    input.days[5].known.as_mut().unwrap()[0].mean_overlap = Some(0.5);
    let report = judge(&input);
    assert_eq!(report.streak, 1);
    assert_eq!(report.verdict, Verdict::Fail);
}

/// Artifacts' recall: vellev at most two points under Weaviate's.
#[test]
fn artifact_recall_allows_two_points() {
    let mut input = artifact_week(100);
    let found = |input: &mut WeekInput, n| {
        for d in &mut input.days {
            d.known.as_mut().unwrap()[0].shadow_within_k = n;
        }
    };
    found(&mut input, 98);
    assert_eq!(judge(&input).verdict, Verdict::Pass);
    found(&mut input, 97);
    let report = judge(&input);
    assert_eq!(report.verdict, Verdict::Fail);
    // Over the week: no day is failed for it.
    assert!(report.days.iter().all(|d| d.verdict == Verdict::Pass));
}

/// Errors and empty-on-vellev are judged over the week by the upper 95 %
/// Wilson bound, never the point rate; a day's figures are information.
#[test]
fn rates_are_judged_over_the_week_by_their_upper_bound() {
    let mut input = memory_week(200, false);
    input.week = live(2000);
    // A bad day shows, and fails nothing by itself.
    input.days[1].live.failed = 3;
    input.days[1].live.samples += 3;
    input.week.failed = 3;
    input.week.samples += 3;
    let report = judge(&input);
    assert_eq!(report.days[1].verdict, Verdict::Pass, "{report:#?}");
    assert!(report.days[1].notes[0].contains("3 of 203"), "{report:#?}");
    assert_eq!(
        week_gate(&report, "errors and timeouts").verdict,
        Verdict::Pass
    );
    // 8 in 2,000 is 0.4 %, under 0.5 %, but its bound is not.
    input.week.failed = 8;
    let report = judge(&input);
    assert_eq!(
        week_gate(&report, "errors and timeouts").verdict,
        Verdict::Fail,
        "{report:#?}"
    );
    assert_eq!(report.verdict, Verdict::Fail);
    // 300 with none empty cannot show 0.5 % at all: too few, not pass.
    let mut input = memory_week(200, false);
    input.week = live(300);
    let report = judge(&input);
    assert_eq!(
        week_gate(&report, "empty on vellev only").verdict,
        Verdict::TooFew,
        "{report:#?}"
    );
    input.week = live(2000);
    input.week.empty_shadow_only = 20;
    assert_eq!(
        week_gate(&judge(&input), "empty on vellev only").verdict,
        Verdict::Fail
    );
    assert!((wilson_upper(3, 2000) - 0.003_75).abs() < 0.000_05);
}

/// Self@1 is a paired McNemar non-inferiority test at the margin, on the
/// gating groups alone, each at its own minimum size.
#[test]
fn self_first_is_a_non_inferiority_test_on_the_gating_groups() {
    let input = |b: i64, c: i64| {
        let mut input = memory_week(143, true);
        input.week = live(999);
        let g = &mut input.days[0].known.as_mut().unwrap()[0];
        g.only_primary_first = b;
        g.only_shadow_first = c;
        g.shadow_first -= b;
        input
    };
    let m2 = |input: &WeekInput| week_gate(&judge(input), "agreement (M2)").verdict;
    // vellev a little behind (3 of 7,000), within half a point: passes,
    // where a point estimate would fail it.
    assert_eq!(m2(&input(3, 0)), Verdict::Pass);
    // 60 behind is 0.86 points: fails.
    assert_eq!(m2(&input(60, 0)), Verdict::Fail);
    // 30 behind net (40 against 10) is 0.43 points, inside the margin on
    // the point estimate, but its lower bound is past it: fails.
    assert_eq!(m2(&input(40, 10)), Verdict::Fail);
    // As many the other way: no worse.
    assert_eq!(m2(&input(60, 60)), Verdict::Pass);
    // The margin is the setting's.
    let mut wider = input(60, 0);
    wider.self_first.margin = 0.02;
    assert_eq!(m2(&wider), Verdict::Pass);

    // The hard mode and alpha 1.0 are reported, never judged.
    let mut reported = memory_week(143, true);
    reported.week = live(999);
    for d in &mut reported.days {
        for (mode, alpha) in [(QueryMode::Hard, 0.75), (QueryMode::Window, 1.0)] {
            let mut g = known(Population::Scoped, 1000);
            g.mode = mode;
            g.alpha = alpha;
            g.only_primary_first = 500;
            g.shadow_first = 500;
            g.mean_overlap = Some(0.1);
            // First in the list, so a group found by population and
            // alpha alone would be this one.
            d.known.as_mut().unwrap().insert(0, g);
        }
    }
    let report = judge(&reported);
    assert_eq!(report.verdict, Verdict::Pass, "{report:#?}");
    assert!(
        week_gate(&report, "agreement (M2)")
            .detail
            .contains("hard alpha 0.75 (reported, not judged)"),
        "{report:#?}"
    );

    // Each gating group needs its minimum over the week: the unscoped
    // population at 80 a day is 560, under 600.
    let mut thin = memory_week(143, true);
    thin.week = live(999);
    for d in &mut thin.days {
        d.known.as_mut().unwrap()[1] = known(Population::Unscoped, 80);
    }
    assert_eq!(m2(&thin), Verdict::TooFew);
    // Under a lower minimum it is still too few: 560 pairs cannot show
    // half a point even with none discordant (736 can).
    thin.self_first.unscoped_min = 500;
    let report = judge(&thin);
    assert_eq!(
        week_gate(&report, "agreement (M2)").verdict,
        Verdict::TooFew
    );
    assert!(
        week_gate(&report, "agreement (M2)")
            .detail
            .contains("cannot show"),
        "{report:#?}"
    );
    for d in &mut thin.days {
        d.known.as_mut().unwrap()[1] = known(Population::Unscoped, 110);
    }
    assert_eq!(m2(&thin), Verdict::Pass);
    // The minimum is on distinct memories: unscoped ones drawn night after
    // night count once.
    thin.distinct.insert(Population::Unscoped, 200);
    thin.self_first.unscoped_min = 600;
    let report = judge(&thin);
    assert_eq!(
        week_gate(&report, "agreement (M2)").verdict,
        Verdict::TooFew
    );
    assert!(
        week_gate(&report, "agreement (M2)")
            .detail
            .contains("200 distinct"),
        "{report:#?}"
    );
    // And a gating group missing altogether is too few.
    for d in &mut thin.days {
        d.known.as_mut().unwrap().truncate(1);
    }
    assert_eq!(m2(&thin), Verdict::TooFew);
}

/// The self@1 test against the exact bound: at 300 pairs with none
/// discordant it is too few (300 cannot show half a point); one each way
/// in 1,000 fails (the bound is -0.70 points); at the reviewer's values.
#[test]
fn self_first_uses_the_exact_bound() {
    let pairs = |n: i64, b: i64, c: i64| KnownItemSummary {
        both_answered: n,
        only_primary_first: b,
        only_shadow_first: c,
        primary_first: n - c,
        shadow_first: n - b,
        ..KnownItemSummary::default()
    };
    let test = |g: &KnownItemSummary| self_first_test(g, 0.005, 1, g.both_answered);
    assert_eq!(test(&pairs(300, 0, 0)).0, Verdict::TooFew);
    assert_eq!(test(&pairs(1000, 1, 1)).0, Verdict::Fail);
    assert!(
        test(&pairs(1000, 1, 1)).1.contains("bound -0.70"),
        "{:?}",
        test(&pairs(1000, 1, 1))
    );
    assert_eq!(test(&pairs(1000, 0, 0)).0, Verdict::Pass);
    assert_eq!(test(&pairs(3000, 10, 10)).0, Verdict::Pass);
    assert_eq!(test(&pairs(1000, 3, 6)).0, Verdict::Fail);
    assert_eq!(test(&pairs(7000, 30, 40)).0, Verdict::Pass);
}

/// The gating group is the one asked at the configured serving alpha; a
/// week whose runs were asked at another (it changed mid-week) is too few.
#[test]
fn the_serving_alpha_comes_from_config_and_a_change_is_too_few() {
    let mut input = memory_week(143, true);
    input.week = live(999);
    input.days[2].run_alpha = Some(0.6);
    let report = judge(&input);
    let m2 = week_gate(&report, "agreement (M2)");
    assert_eq!(m2.verdict, Verdict::TooFew, "{report:#?}");
    assert!(m2.detail.contains("changed mid-week"), "{m2:?}");
    // Configured at 1.0, the 0.75 groups are not the gating ones.
    let mut input = memory_week(143, true);
    input.week = live(999);
    input.serving_alpha = 1.0;
    for d in &mut input.days {
        d.run_alpha = Some(1.0);
    }
    assert_eq!(
        week_gate(&judge(&input), "agreement (M2)").verdict,
        Verdict::TooFew
    );
}

/// A day's live agreement under its minimum is information, not judged:
/// the day neither fails nor restarts the count.
#[test]
fn a_thin_day_s_agreement_is_not_judged() {
    let mut input = memory_week(1000, false);
    input.week = live(6050);
    input.days[3].live = live(50);
    input.days[3].live.mean_rbo = Some(0.5);
    let report = judge(&input);
    assert_eq!(report.days[3].verdict, Verdict::NotJudged, "{report:#?}");
    assert_eq!(report.streak, 7);
    assert_eq!(report.verdict, Verdict::Pass);
    input.days[3].live = live(100);
    input.days[3].live.mean_rbo = Some(0.5);
    assert_eq!(judge(&input).days[3].verdict, Verdict::Fail);
}

/// Artifacts' recall gates on the window mode at the serving alpha over at
/// least 300 items; other groups are reported; the judge off is not
/// judged, never a pass.
#[test]
fn artifact_recall_has_a_minimum_and_one_gating_group() {
    let mut input = artifact_week(100);
    for d in &mut input.days {
        let g = &mut d.known.as_mut().unwrap()[0];
        *g = KnownItemSummary {
            alpha: 1.0,
            ..known(Population::Scoped, 40)
        };
    }
    let report = judge(&input);
    assert_eq!(
        week_gate(&report, "known-item recall@10").verdict,
        Verdict::TooFew,
        "{report:#?}"
    );
    let mut input = artifact_week(100);
    for d in &mut input.days {
        let mut hard = KnownItemSummary {
            alpha: 1.0,
            mode: QueryMode::Hard,
            ..known(Population::Scoped, 100)
        };
        hard.shadow_within_k = 0;
        // Another alpha, which sorts before 1.0 once the days are summed.
        let mut other = KnownItemSummary {
            alpha: 0.5,
            ..known(Population::Scoped, 100)
        };
        other.shadow_within_k = 0;
        d.known.as_mut().unwrap().insert(0, hard);
        d.known.as_mut().unwrap().insert(0, other);
    }
    let report = judge(&input);
    assert_eq!(
        week_gate(&report, "known-item recall@10").verdict,
        Verdict::Pass,
        "{report:#?}"
    );
    assert_eq!(
        week_gate(&report, "judged nDCG@5").verdict,
        Verdict::NotJudged
    );
}

/// Fewer than seven days are too few, however good.
#[test]
fn fewer_than_seven_days_are_too_few() {
    let mut input = memory_week(200, false);
    input.days.truncate(6);
    assert_eq!(judge(&input).verdict, Verdict::TooFew);
}

/// What cannot be judged from Postgres is listed, whatever the verdict.
#[test]
fn what_is_not_judged_is_said() {
    let report = judge(&memory_week(200, false));
    assert!(report
        .not_judged
        .iter()
        .any(|n| n.contains("restore drill")));
    let text = to_text(&report);
    assert!(text.contains("not judged here"), "{text}");
    let json = to_json(&report);
    assert_eq!(json["verdict"], "pass");
    assert_eq!(json["not_judged"].as_array().unwrap().len(), 4);
}

/// Groups summed over days: counts added, means weighted by items.
#[test]
fn known_groups_sum_over_days_by_weight() {
    let mut a = known(Population::Scoped, 100);
    a.mean_overlap = Some(1.0);
    let mut b = known(Population::Scoped, 300);
    b.mean_overlap = Some(0.6);
    let c = known(Population::Unscoped, 10);
    let summed = sum_groups([vec![a, c].as_slice(), vec![b].as_slice()]);
    assert_eq!(summed.len(), 2);
    assert_eq!(summed[0].both_answered, 400);
    assert!((summed[0].mean_overlap.unwrap() - 0.7).abs() < 1e-9);
    assert_eq!(summed[1].population, Population::Unscoped);
}

/// Retention: the comparisons are deleted only after their weeks'
/// verdicts are recorded; a failure to record leaves them be.
#[test]
fn the_prune_never_runs_past_an_unrecorded_week() {
    let pruned = std::cell::Cell::new(false);
    let out = verdicts_first(
        || Err(eyre::eyre!("postgres went away")),
        || {
            pruned.set(true);
            Ok(5)
        },
    );
    assert!(out.is_err());
    assert!(!pruned.get(), "pruned with a week unrecorded");
    assert_eq!(verdicts_first(|| Ok(1), || Ok(5)).unwrap(), 5);
}

#[test]
fn a_week_starts_on_monday() {
    // 2026-10-12 is a Monday.
    assert_eq!(week_of(day(0)), day(0));
    assert_eq!(week_of(day(6)), day(0));
    assert_eq!(week_of(day(7)), day(7));
}

#[cfg(feature = "postgres")]
mod against_postgres {
    use super::*;
    use std::sync::Arc;
    use workforce_common::shadow_postgres::ShadowStore;

    /// `test` over the shadow tables of a fresh database.
    async fn with_tables<F, Fut>(test: F)
    where
        F: FnOnce(Arc<ShadowStore>, workforce_common::postgres::PostgresPool) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        workforce_postgres::test_support::with_test_pool(
            workforce_common::telemetry_postgres::PostgresTelemetryStore::build_pool,
            |pool| async move {
                let tables = ShadowStore::with_pool(pool.clone()).await.unwrap();
                test(tables, pool).await
            },
        )
        .await;
    }

    async fn insert(
        pool: &workforce_common::postgres::PostgresPool,
        store: &str,
        at: DateTime<Utc>,
        outcome: &str,
        primary_ms: f64,
        shadow_ms: f64,
    ) {
        let conn = pool.get().await.unwrap();
        conn.execute(
            "INSERT INTO shadow_comparisons (store, at, outcome, probe, primary_ms,
                 primary_count, shadow_ms, shadow_count, k, overlap, jaccard, rbo, top1,
                 empty_primary_only, empty_shadow_only)
             VALUES ($1, $2, $3, false, $4, 10, $5, 10, 10, 1.0, 1.0, 1.0, true, false, false)",
            &[&store, &at, &outcome, &primary_ms, &shadow_ms],
        )
        .await
        .unwrap();
    }

    /// Rule 4: a week's p99 is the p99 of its raw rows. Here each day's
    /// p99 has vellev under Weaviate but one day carries vellev's slow tail:
    /// the mean of the daily p99s passes, the week's own p99 fails.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_week_s_p99_comes_from_its_raw_rows() {
        with_tables(|tables, pool| async move {
            let from = day(0);
            for n in 0..7 {
                let at = day_start(day(n)) + Days::hours(1);
                for i in 0..100 {
                    // Weaviate steady at 50 ms. vellev at 10 ms, except on the
                    // first day, where 30 of its 100 take 60 ms: that day's p99
                    // is 60 (over Weaviate's 50), the others' 10.
                    let shadow = if n == 0 && i < 30 { 60.0 } else { 10.0 };
                    insert(&pool, "memories", at, "answered", 50.0, shadow).await;
                }
            }
            let config = WorkforceConfig::default();
            let input = gather(&tables, &config, "memories", from).await.unwrap();
            let mean_of_days: f64 = input
                .days
                .iter()
                .map(|d| d.live.shadow_wt.p99.unwrap())
                .sum::<f64>()
                / 7.0;
            assert!(
                mean_of_days < 50.0,
                "the fixture's daily p99s average under 50: {mean_of_days}"
            );
            // 30 of 700 slow is over 1 %: the week's p99 is 60.
            assert_eq!(input.week.shadow_wt.p99, Some(60.0), "{:?}", input.week);
            assert_eq!(input.week.samples, 700);
            let report = judge(&input);
            assert_eq!(
                week_gate(&report, "latency (L1)").verdict,
                Verdict::Fail,
                "{report:#?}"
            );
        })
        .await;
    }

    /// Rule 12: one run a store and day counts, the one that ran to its
    /// end; a day whose only run stopped part way has none.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_a_run_that_ran_to_its_end_counts() {
        with_tables(|tables, pool| async move {
            let conn = pool.get().await.unwrap();
            let run = |day: NaiveDate, outcome: &'static str| {
                let conn = &conn;
                async move {
                    conn.query_one(
                        "INSERT INTO shadow_known_item_runs (store, run_day, base_seed, run_seed,
                             derivation, k, alpha_serving, items_wanted, outcome)
                         VALUES ('memories', $1, 0, '0', 1, 10, 0.75, 10, $2) RETURNING id",
                        &[&day, &outcome],
                    )
                    .await
                    .unwrap()
                    .get::<_, i64>(0)
                }
            };
            run(day(0), "incomplete").await;
            let ok = run(day(0), "ok").await;
            run(day(1), "incomplete").await;
            run(day(2), "abandoned").await;
            assert_eq!(
                tables
                    .counted_known_item_run("memories", day(0))
                    .await
                    .unwrap(),
                Some((ok, 0.75))
            );
            assert_eq!(
                tables
                    .counted_known_item_run("memories", day(1))
                    .await
                    .unwrap(),
                None
            );
            assert_eq!(
                tables
                    .counted_known_item_run("memories", day(2))
                    .await
                    .unwrap(),
                None
            );
            assert_eq!(
                tables
                    .counted_known_item_run("artifacts", day(0))
                    .await
                    .unwrap(),
                None
            );
        })
        .await;
    }

    /// Whole weeks with comparisons get their verdict recorded, once; the
    /// week still running does not.
    #[tokio::test(flavor = "multi_thread")]
    async fn whole_weeks_are_recorded_once() {
        with_tables(|tables, pool| async move {
            insert(&pool, "memories", day_start(day(2)), "answered", 5.0, 4.0).await;
            insert(&pool, "artifacts", day_start(day(8)), "answered", 5.0, 4.0).await;
            let config = WorkforceConfig::default();
            // On day 9, the week of day 0 is whole; that of day 7 is not.
            assert_eq!(
                record_due_verdicts(&tables, &config, day(9)).await.unwrap(),
                1
            );
            let recorded = tables.verdicts(None, 10).await.unwrap();
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].store, "memories");
            assert_eq!(recorded[0].week_start, day(0));
            assert_eq!(recorded[0].week_end, day(6));
            assert_eq!(recorded[0].verdict, "too_few");
            assert_eq!(recorded[0].report["store"], "memories");
            assert_eq!(
                record_due_verdicts(&tables, &config, day(9)).await.unwrap(),
                0
            );
            assert_eq!(
                record_due_verdicts(&tables, &config, day(14))
                    .await
                    .unwrap(),
                1
            );
        })
        .await;
    }

    /// PR 9b's labels, where rows carry them, give where vellev's time
    /// went: its queue and index call, and its time alone against in a
    /// burst. Rows without them are left out of the parts, not counted 0.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_latency_breakdown_reads_9b_s_labels() {
        with_tables(|tables, pool| async move {
            let conn = pool.get().await.unwrap();
            let at = day_start(day(0)) + Days::hours(2);
            for (shadow_ms, labels) in [
                (4.0, serde_json::json!({"shadow_queue_ms": 1.0, "shadow_index_ms": 3.0, "concurrent": 1})),
                (6.0, serde_json::json!({"shadow_queue_ms": 1.0, "shadow_index_ms": 5.0, "concurrent": 1})),
                (90.0, serde_json::json!({"shadow_queue_ms": 30.0, "shadow_index_ms": 80.0, "concurrent": 4})),
                (7.0, serde_json::json!({"same_model": "true"})),
            ] {
                conn.execute(
                    "INSERT INTO shadow_comparisons (store, at, outcome, probe, labels, primary_ms,
                         primary_count, shadow_ms, shadow_count)
                     VALUES ('memories', $1, 'answered', false, $2, 10, 10, $3, 10)",
                    &[&at, &labels, &shadow_ms],
                )
                .await
                .unwrap();
            }
            let w = tables
                .window_stats("memories", day_start(day(0)), day_start(day(1)))
                .await
                .unwrap();
            let b = w.parts;
            assert_eq!(b.labelled, 3, "{b:?}");
            assert_eq!((b.alone, b.burst), (2, 1), "{b:?}");
            assert_eq!(b.alone_p50, Some(5.0));
            assert_eq!(b.burst_p50, Some(90.0));
            assert_eq!(b.queue_p50, Some(1.0));
            assert_eq!(b.index_p50, Some(5.0));
            let text = parts_text(&b).unwrap();
            assert!(text.contains("in a burst (1)"), "{text}");
            assert_eq!(parts_text(&Breakdown::default()), None);
        })
        .await;
    }

    /// The backlog is read on the shadow tables' own pool.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_backlog_is_read_on_the_shadow_tables_pool() {
        with_tables(|tables, pool| async move {
            assert_eq!(tables.mirror_backlog().await.unwrap().pending, 0);
            pool.get()
                .await
                .unwrap()
                .execute(
                    "INSERT INTO memory_mirror_pending (memory_id) VALUES ('m1'), ('m2')",
                    &[],
                )
                .await
                .unwrap();
            let b = tables.mirror_backlog().await.unwrap();
            assert_eq!(b.pending, 2);
            assert!(b.oldest.is_some());
        })
        .await;
    }

    /// A reader opens the tables read only: on a database no hub has
    /// built, it declines and creates nothing; once built, it reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reader_never_builds_the_tables() {
        workforce_postgres::test_support::with_test_pool(
            workforce_common::telemetry_postgres::PostgresTelemetryStore::build_pool,
            |pool| async move {
                let declined = ShadowStore::read_only(pool.clone()).await;
                let said = declined.err().expect("declined").to_string();
                assert!(said.contains("not built yet"), "{said}");
                let built: bool = pool
                    .get()
                    .await
                    .unwrap()
                    .query_one("SELECT to_regclass('shadow_comparisons') IS NOT NULL", &[])
                    .await
                    .unwrap()
                    .get(0);
                assert!(!built, "the reader built the tables");
                ShadowStore::with_pool(pool.clone()).await.unwrap();
                let reader = ShadowStore::read_only(pool).await.unwrap();
                assert!(reader.verdicts(None, 1).await.unwrap().is_empty());
            },
        )
        .await;
    }

    /// An item drawn on two nights counts once among the distinct.
    #[tokio::test(flavor = "multi_thread")]
    async fn distinct_items_count_a_redrawn_memory_once() {
        with_tables(|tables, pool| async move {
            let conn = pool.get().await.unwrap();
            let mut runs = Vec::new();
            for n in 0..2 {
                let run: i64 = conn
                    .query_one(
                        "INSERT INTO shadow_known_item_runs (store, run_day, base_seed, run_seed,
                             derivation, k, alpha_serving, items_wanted, outcome)
                         VALUES ('memories', $1, 0, '0', 1, 10, 0.75, 10, 'ok') RETURNING id",
                        &[&day(n)],
                    )
                    .await
                    .unwrap()
                    .get(0);
                for item in ["m1", "m2"] {
                    let item = if n == 1 && item == "m2" { "m3" } else { item };
                    conn.execute(
                        "INSERT INTO shadow_known_items (run_id, item_id, alpha, status,
                             population, mode)
                         VALUES ($1, $2, 0.75, 'asked', 'unscoped', 'window')",
                        &[&run, &item],
                    )
                    .await
                    .unwrap();
                }
                runs.push(run);
            }
            let n = tables
                .distinct_known_items(&runs, "unscoped", "window", 0.75)
                .await
                .unwrap();
            assert_eq!(n, 3, "m1 twice, m2 and m3 once");
        })
        .await;
    }
}
