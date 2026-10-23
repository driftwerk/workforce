//! What a run amounts to, in two forms. [`Scores`] is counts, shares and
//! medians per arm and use case, with paired differences between arms. It
//! holds numbers under field names and use-case labels and nothing else (no
//! question, answer, reason, nor the name of a set, arm, agent or model), so
//! it may leave the host. [`Full`] names the arms and adds every question
//! with each arm's answer and grade, and stays where the results are.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::Serialize;
use workforce_hub::eval_grader::{Grade, GradeOutcome, Verdict};

use super::results::{ArmEntry, GradeRecord, Judge, Results, TurnRecord, TurnStatus, GRADE_PASSES};
use super::set::{EvalSet, Question};

/// The slice that holds every use case.
const ALL: &str = "all";

/// Below this an arm's difference from the baseline counts as
/// distinguishable from noise.
const SIGNIFICANCE: f64 = 0.05;

/// The fewest differently-scored questions a two-sided sign test can call
/// significant: with five, the most lopsided split still has p = 0.0625.
const MIN_UNTIED_PAIRS: usize = 6;

const TEST: &str = "exact two-sided sign test on the questions both arms were graded on, \
                    ordering correct > partly > wrong; questions scored the same are left \
                    out. The line for all questions is the result. The lines per use case are \
                    tests of their own, not corrected for being several: read them as pointers. \
                    'Not distinguishable' is not 'the same': with this few questions only a \
                    large difference can show";

/// One arm's go at one question.
struct Row<'a> {
    question: &'a Question,
    turn: Option<&'a TurnRecord>,
    readings: Vec<Option<&'a GradeRecord>>,
}

fn grade_of(reading: Option<&GradeRecord>) -> Option<&Grade> {
    match &reading?.outcome {
        GradeOutcome::Graded(grade) => Some(grade),
        _ => None,
    }
}

impl<'a> Row<'a> {
    fn answered(&self) -> Option<&'a TurnRecord> {
        self.turn.filter(|turn| turn.status == TurnStatus::Answered)
    }

    /// The grade that counts: the judge's first reading. The rule is fixed
    /// before any answer is read and falls on every arm alike; dropping the
    /// answers the second reading disagrees on would drop the borderline
    /// ones, and not evenly across arms. Disagreements are counted instead.
    fn grade(&self) -> Option<&'a Grade> {
        grade_of(self.readings.first().copied().flatten())
    }

    /// Whether the judge's repeat reading gave the same verdict, when both
    /// readings left one.
    fn judge_agrees(&self) -> Option<bool> {
        let repeat = grade_of(self.readings.get(1).copied().flatten())?;
        Some(self.grade()?.verdict == repeat.verdict)
    }
}

fn rows<'a>(results: &'a Results, set: &'a EvalSet, arm: &ArmEntry) -> Vec<Row<'a>> {
    let turns = results.current_turns(arm, set);
    set.questions
        .iter()
        .map(|question| {
            let turn = turns.get(question.id.as_str()).copied();
            Row {
                question,
                turn,
                readings: (1..=GRADE_PASSES)
                    .map(|pass| turn.and_then(|turn| results.grade(&turn.turn_id, pass)))
                    .collect(),
            }
        })
        .collect()
}

fn share(part: usize, whole: usize) -> Option<f64> {
    (whole > 0).then(|| part as f64 / whole as f64)
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    match values.len() {
        0 => None,
        n if n % 2 == 1 => Some(values[mid]),
        _ => Some((values[mid - 1] + values[mid]) / 2.0),
    }
}

/// One arm's results on one slice of the set. Every share is of `asked`,
/// except `cited` and `fell_for_trap`, which are of the graded answers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tally {
    pub questions: usize,
    pub asked: usize,
    pub correct: usize,
    pub partly: usize,
    pub wrong: usize,
    pub ungraded: usize,
    pub failed: usize,
    /// Answers that are the wind-down of a turn stopped at its round cap.
    /// They are graded like any other answer.
    pub capped: usize,
    pub correct_share: Option<f64>,
    pub partly_share: Option<f64>,
    pub wrong_share: Option<f64>,
    pub ungraded_share: Option<f64>,
    pub failed_share: Option<f64>,
    pub capped_share: Option<f64>,
    pub cited_share: Option<f64>,
    pub fell_for_trap_share: Option<f64>,
    pub median_cost_usd: Option<f64>,
    pub median_rounds: Option<f64>,
    pub median_seconds: Option<f64>,
    /// The share of twice-graded answers the judge gave the same verdict
    /// both times, how many those were, and how many it did not.
    pub judge_agreement: Option<f64>,
    pub judge_agreement_of: usize,
    pub judge_disagreed: usize,
    /// How many different `config.toml`s the answers ran under: more than
    /// one means the agent's settings changed while the arm was asked.
    pub configs: usize,
}

impl Tally {
    fn of(rows: &[&Row<'_>]) -> Self {
        let asked = rows.iter().filter(|row| row.turn.is_some()).count();
        let answered: Vec<&TurnRecord> = rows.iter().filter_map(|row| row.answered()).collect();
        let grades: Vec<&Grade> = rows.iter().filter_map(|row| row.grade()).collect();
        let verdicts = |verdict| grades.iter().filter(|g| g.verdict == verdict).count();
        let (correct, partly, wrong) = (
            verdicts(Verdict::Correct),
            verdicts(Verdict::Partly),
            verdicts(Verdict::Wrong),
        );
        let ungraded = answered.len() - grades.len();
        let failed = asked - answered.len();
        let capped = answered.iter().filter(|turn| turn.capped).count();
        let agreements: Vec<bool> = rows.iter().filter_map(|row| row.judge_agrees()).collect();
        let agreed = agreements.iter().filter(|agreed| **agreed).count();
        let metric = |of: fn(&TurnRecord) -> Option<f64>| {
            median(answered.iter().filter_map(|turn| of(turn)).collect())
        };
        Self {
            questions: rows.len(),
            asked,
            correct,
            partly,
            wrong,
            ungraded,
            failed,
            capped,
            correct_share: share(correct, asked),
            partly_share: share(partly, asked),
            wrong_share: share(wrong, asked),
            ungraded_share: share(ungraded, asked),
            failed_share: share(failed, asked),
            capped_share: share(capped, asked),
            cited_share: share(grades.iter().filter(|g| g.cited).count(), grades.len()),
            fell_for_trap_share: share(
                grades.iter().filter(|g| g.fell_for_trap).count(),
                grades.len(),
            ),
            median_cost_usd: metric(|turn| turn.metrics.as_ref().map(|m| m.cost_usd)),
            median_rounds: metric(|turn| turn.metrics.as_ref().map(|m| f64::from(m.rounds))),
            median_seconds: metric(|turn| Some(turn.wall_ms as f64 / 1000.0)),
            judge_agreement: share(agreed, agreements.len()),
            judge_agreement_of: agreements.len(),
            judge_disagreed: agreements.len() - agreed,
            configs: answered
                .iter()
                .map(|turn| turn.config.as_deref())
                .collect::<BTreeSet<_>>()
                .len(),
        }
    }
}

/// Each slice of the set: every use case that has questions, and all of
/// them.
fn slices<'r, 'a, T>(rows: &'r [Row<'a>], of: impl Fn(&[&'r Row<'a>]) -> T) -> BTreeMap<String, T> {
    let mut by_use_case: BTreeMap<&str, Vec<&Row<'a>>> = BTreeMap::new();
    for row in rows {
        by_use_case
            .entry(row.question.use_case.as_str())
            .or_default()
            .push(row);
    }
    by_use_case
        .into_iter()
        .chain([(ALL, rows.iter().collect())])
        .map(|(slice, rows)| (slice.to_string(), of(&rows)))
        .collect()
}

/// The probability of a split at least as lopsided as `better` to `worse`
/// if either were as likely: the exact two-sided sign test.
fn sign_test(better: usize, worse: usize) -> Option<f64> {
    let untied = better + worse;
    (untied > 0).then(|| {
        let tail: f64 = (0..=better.min(worse))
            .scan(0.5f64.powi(untied as i32), |term, i| {
                let this = *term;
                *term *= (untied - i) as f64 / (i + 1) as f64;
                Some(this)
            })
            .sum();
        (2.0 * tail).min(1.0)
    })
}

/// An arm against the baseline, question by question, on one slice.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Paired {
    /// Questions both arms were graded on.
    pub pairs: usize,
    pub better: usize,
    pub worse: usize,
    pub same: usize,
    /// Questions either arm failed, was not asked, or went ungraded on.
    pub left_out: usize,
    pub p_value: Option<f64>,
    /// Pairs in which the judge's second reading of either answer gave
    /// another verdict than its first, and the test without them.
    pub unstable: usize,
    pub p_value_stable: Option<f64>,
    /// Below the threshold on every pair, and still below it without the
    /// unstable ones.
    pub distinguishable: bool,
    /// Medians of the arm's value less the baseline's, over the questions
    /// both answered.
    pub median_cost_usd_diff: Option<f64>,
    pub median_rounds_diff: Option<f64>,
    pub median_seconds_diff: Option<f64>,
}

impl Paired {
    fn of(baseline: &[&Row<'_>], arm: &[&Row<'_>]) -> Self {
        let both = || baseline.iter().zip(arm);
        let graded: Vec<(Verdict, Verdict, bool)> = both()
            .filter_map(|(base, arm)| {
                let unstable = [base, arm]
                    .iter()
                    .any(|row| row.judge_agrees() == Some(false));
                Some((base.grade()?.verdict, arm.grade()?.verdict, unstable))
            })
            .collect();
        let split = |stable_only: bool| {
            let counted = || {
                graded
                    .iter()
                    .filter(move |(_, _, unstable)| !(stable_only && *unstable))
            };
            (
                counted().filter(|(base, arm, _)| arm > base).count(),
                counted().filter(|(base, arm, _)| arm < base).count(),
            )
        };
        let (better, worse) = split(false);
        let (better_stable, worse_stable) = split(true);
        let below = |p: Option<f64>| p.is_some_and(|p| p < SIGNIFICANCE);
        let p_value = sign_test(better, worse);
        let p_value_stable = sign_test(better_stable, worse_stable);
        let diff = |of: fn(&TurnRecord) -> Option<f64>| {
            median(
                both()
                    .filter_map(|(base, arm)| Some(of(arm.answered()?)? - of(base.answered()?)?))
                    .collect(),
            )
        };
        Self {
            pairs: graded.len(),
            better,
            worse,
            same: graded.len() - better - worse,
            left_out: baseline.len() - graded.len(),
            p_value,
            unstable: graded.iter().filter(|(_, _, unstable)| *unstable).count(),
            p_value_stable,
            distinguishable: below(p_value) && below(p_value_stable),
            median_cost_usd_diff: diff(|turn| turn.metrics.as_ref().map(|m| m.cost_usd)),
            median_rounds_diff: diff(|turn| turn.metrics.as_ref().map(|m| f64::from(m.rounds))),
            median_seconds_diff: diff(|turn| Some(turn.wall_ms as f64 / 1000.0)),
        }
    }

    /// The comparison in a sentence or two.
    fn finding(&self) -> String {
        let Some(p) = self.p_value else {
            return "No question was scored differently: there is no difference to test.".into();
        };
        let unstable =
            match self.unstable {
                0 => String::new(),
                n => format!(
                " {n} of the pairs rest on an answer the judge graded differently on its second \
                 reading; without them {}.",
                self.p_value_stable
                    .map_or_else(|| "nothing is left to test".into(), |p| format!("p = {p:.3}"))
            ),
            };
        let few = match self.better + self.worse < MIN_UNTIED_PAIRS {
            true => format!(
                " Fewer than {MIN_UNTIED_PAIRS} differently-scored questions can never reach p < \
                 {SIGNIFICANCE}."
            ),
            false => String::new(),
        };
        format!(
            "Better on {}, worse on {}; sign test p = {p:.3}: {} from noise at this n.{unstable}{few}",
            self.better,
            self.worse,
            match self.distinguishable {
                true => "distinguishable",
                false => "not distinguishable",
            }
        )
    }
}

/// A run as numbers only: arms by position, the first being the baseline,
/// and each later arm's comparison with it in the same order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Scores {
    pub questions: usize,
    pub arms: Vec<BTreeMap<String, Tally>>,
    pub comparisons: Vec<BTreeMap<String, Paired>>,
    /// Recorded turns left out because their question or arm has changed
    /// since.
    pub stale_turns: usize,
    /// What every recorded turn's own model calls cost, left-out turns
    /// included. The reviews the hub runs after an answer are billed besides
    /// and are not in this figure, nor in any per-turn cost.
    pub turns_cost_usd: f64,
    /// What every reading by the judge cost.
    pub grading_cost_usd: f64,
    /// How many different models judged: more than one means the judge's
    /// agent changed its model during the run.
    pub judge_models: usize,
}

#[derive(Debug, Serialize)]
pub struct ArmDetail {
    pub status: &'static str,
    pub answer: Option<String>,
    pub error: Option<String>,
    pub capped: bool,
    pub grade: Option<Grade>,
    pub ungraded_because: Option<String>,
    pub repeat_verdict: Option<Verdict>,
    pub judge_agrees: Option<bool>,
    pub cost_usd: Option<f64>,
    pub rounds: Option<u32>,
    pub seconds: Option<f64>,
    pub model: Option<String>,
    pub tools: Vec<String>,
    pub config: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct QuestionDetail {
    pub id: String,
    pub version: String,
    pub use_case: &'static str,
    pub question: String,
    pub expected: String,
    pub arms: BTreeMap<String, ArmDetail>,
}

/// A run with its names, and every question, answer and grade.
#[derive(Debug, Serialize)]
pub struct Full {
    pub set: String,
    pub judge: Judge,
    /// The arms, in the order [`Scores`] holds them.
    pub arms: Vec<ArmEntry>,
    pub scores: Scores,
    pub questions: Vec<QuestionDetail>,
}

impl ArmDetail {
    fn of(row: &Row<'_>) -> Self {
        let metrics = row.turn.and_then(|turn| turn.metrics.as_ref());
        Self {
            status: match row.turn.map(|turn| turn.status) {
                None => "not_asked",
                Some(TurnStatus::Answered) => "answered",
                Some(TurnStatus::Failed) => "failed",
            },
            answer: row.answered().map(|turn| turn.answer.clone()),
            error: row.turn.and_then(|turn| turn.error.clone()),
            capped: row.turn.is_some_and(|turn| turn.capped),
            grade: row.grade().cloned(),
            ungraded_because: row
                .readings
                .first()
                .copied()
                .flatten()
                .and_then(|read| match &read.outcome {
                    GradeOutcome::Ungraded { reason } => Some(reason.clone()),
                    _ => None,
                }),
            repeat_verdict: grade_of(row.readings.get(1).copied().flatten()).map(|g| g.verdict),
            judge_agrees: row.judge_agrees(),
            cost_usd: metrics.map(|m| m.cost_usd),
            rounds: metrics.map(|m| m.rounds),
            seconds: row.turn.map(|turn| turn.wall_ms as f64 / 1000.0),
            model: metrics.map(|m| m.model.clone()),
            tools: metrics.map(|m| m.tools.clone()).unwrap_or_default(),
            config: row.turn.and_then(|turn| turn.config.clone()),
            session_id: row.turn.and_then(|turn| turn.session_id.clone()),
        }
    }
}

/// Both forms of what `results` hold for `set` as it is now.
pub fn report(results: &Results, set: &EvalSet) -> Full {
    let arms = &results.manifest.arms;
    let rows: Vec<Vec<Row<'_>>> = arms.iter().map(|arm| rows(results, set, arm)).collect();
    let scores = Scores {
        questions: set.questions.len(),
        arms: rows.iter().map(|rows| slices(rows, Tally::of)).collect(),
        comparisons: rows
            .iter()
            .skip(1)
            .map(|arm_rows| {
                let baseline: BTreeMap<String, Vec<&Row<'_>>> =
                    slices(&rows[0], |rows| rows.to_vec());
                slices(arm_rows, |rows| rows.to_vec())
                    .into_iter()
                    .map(|(slice, rows)| {
                        let paired = Paired::of(&baseline[&slice], &rows);
                        (slice, paired)
                    })
                    .collect()
            })
            .collect(),
        stale_turns: results.stale_turns(set),
        turns_cost_usd: results
            .turns
            .iter()
            .filter_map(|turn| turn.metrics.as_ref())
            .map(|metrics| metrics.cost_usd)
            .sum(),
        grading_cost_usd: results.grades.iter().map(|grade| grade.cost_usd).sum(),
        judge_models: results
            .grades
            .iter()
            .filter_map(|grade| grade.model.as_deref())
            .collect::<BTreeSet<_>>()
            .len(),
    };
    let questions = set
        .questions
        .iter()
        .enumerate()
        .map(|(n, question)| QuestionDetail {
            id: question.id.clone(),
            version: question.hash(),
            use_case: question.use_case.as_str(),
            question: question.question.clone(),
            expected: question.expected.clone(),
            arms: arms
                .iter()
                .zip(&rows)
                .map(|(arm, rows)| (arm.name.clone(), ArmDetail::of(&rows[n])))
                .collect(),
        })
        .collect();
    Full {
        set: set.name.clone(),
        judge: results.manifest.judge.clone(),
        arms: arms.clone(),
        scores,
        questions,
    }
}

fn percent(share: Option<f64>) -> String {
    share.map_or_else(|| "-".to_string(), |s| format!("{:.0}%", s * 100.0))
}

fn number(value: Option<f64>, decimals: usize) -> String {
    value.map_or_else(|| "-".to_string(), |v| format!("{v:.decimals$}"))
}

fn signed(value: Option<f64>, decimals: usize) -> String {
    value.map_or_else(|| "-".to_string(), |v| format!("{v:+.decimals$}"))
}

/// A report's slices in the order they are read: all questions, then each
/// use case.
fn in_reading_order<T>(slices: &BTreeMap<String, T>) -> impl Iterator<Item = (&str, &T)> {
    slices
        .get_key_value(ALL)
        .into_iter()
        .chain(slices.iter().filter(|(slice, _)| *slice != ALL))
        .map(|(slice, of)| (slice.as_str(), of))
}

impl Full {
    /// The scores as a table for a terminal, with the arms named. It holds
    /// nothing a question or an answer said.
    pub fn render(&self) -> String {
        let scores = &self.scores;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Set {} ({} questions), judged on {}{}",
            self.set,
            scores.questions,
            self.judge.agent,
            self.judge
                .model
                .as_ref()
                .map(|model| format!(" ({model})"))
                .unwrap_or_default()
        );
        for (arm, slices) in self.arms.iter().zip(&scores.arms) {
            let _ = writeln!(
                out,
                "\nArm {} (agent {}{})",
                arm.name,
                arm.agent,
                match arm.overridden {
                    true => ", with overrides",
                    false => "",
                }
            );
            let _ = writeln!(
                out,
                "  {:<14} {:>3} {:>8} {:>7} {:>6} {:>9} {:>7} {:>7} {:>6} {:>5} {:>8} {:>7} {:>6}  judge agrees",
                "use case", "n", "correct", "partly", "wrong", "ungraded", "failed", "capped",
                "cited", "trap", "cost $", "rounds", "secs"
            );
            for (slice, tally) in in_reading_order(slices) {
                let _ = writeln!(
                    out,
                    "  {:<14} {:>3} {:>8} {:>7} {:>6} {:>9} {:>7} {:>7} {:>6} {:>5} {:>8} {:>7} {:>6}  {} of {}",
                    slice,
                    tally.asked,
                    percent(tally.correct_share),
                    percent(tally.partly_share),
                    percent(tally.wrong_share),
                    percent(tally.ungraded_share),
                    percent(tally.failed_share),
                    percent(tally.capped_share),
                    percent(tally.cited_share),
                    percent(tally.fell_for_trap_share),
                    number(tally.median_cost_usd, 3),
                    number(tally.median_rounds, 1),
                    number(tally.median_seconds, 0),
                    percent(tally.judge_agreement),
                    tally.judge_agreement_of,
                );
            }
            if let Some(configs) = slices.get(ALL).map(|all| all.configs).filter(|n| *n > 1) {
                let _ = writeln!(
                    out,
                    "  Its answers ran under {configs} different configs: the agent's settings \
                     changed while it was asked."
                );
            }
        }
        for (arm, slices) in self.arms.iter().skip(1).zip(&scores.comparisons) {
            let _ = writeln!(out, "\n{} against {}", arm.name, self.arms[0].name);
            for (slice, paired) in in_reading_order(slices) {
                let _ = writeln!(
                    out,
                    "  {:<14} {} pairs ({} left out), {} same. Median cost {} $, rounds {}, \
                     seconds {}.\n  {:<14} {}",
                    slice,
                    paired.pairs,
                    paired.left_out,
                    paired.same,
                    signed(paired.median_cost_usd_diff, 3),
                    signed(paired.median_rounds_diff, 1),
                    signed(paired.median_seconds_diff, 0),
                    "",
                    paired.finding(),
                );
            }
        }
        if !scores.comparisons.is_empty() {
            let _ = writeln!(out, "\nTest: {TEST}.");
        }
        let _ = writeln!(
            out,
            "\nShares are of the questions asked (n); cited and trap are of the graded answers; \
             cost, rounds and seconds are medians over answered turns. The grade is the judge's \
             first reading; 'judge agrees' is how often its second gave the same verdict. \
             'capped' answers are the wind-down of a turn stopped at its round cap.\n\
             A turn's cost is its own model calls: the reviews the hub runs after an answer, and \
             the grading, are billed besides. Turns cost ${:.2} in all; grading cost ${:.2}.",
            scores.turns_cost_usd, scores.grading_cost_usd
        );
        if scores.stale_turns > 0 {
            let _ = writeln!(
                out,
                "{} recorded turns were left out: their question or arm has changed since.",
                scores.stale_turns
            );
        }
        if scores.judge_models > 1 {
            let _ = writeln!(
                out,
                "{} different models judged this run: the judge's agent changed its model \
                 part-way, so grades are not all one judge's.",
                scores.judge_models
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evals::arms::Arm;
    use crate::evals::results::tests::{graded, judge, turn};
    use crate::evals::results::{GradeRecord, Manifest, TurnMetrics};
    use crate::evals::set::tests::example_dir;
    use chrono::Utc;

    fn arms() -> [Arm; 2] {
        let copy = Arm {
            name: "candidate".into(),
            agent: "analyst-eval".into(),
            copy_of: Some("analyst".into()),
            overrides: toml::Table::new(),
        };
        [Arm::of_agent("analyst"), copy]
    }

    fn results(turns: Vec<TurnRecord>, grades: Vec<GradeRecord>) -> Results {
        Results {
            manifest: Manifest {
                run_id: "run".into(),
                set: "example".into(),
                arms: arms().iter().map(ArmEntry::from).collect(),
                judge: judge("analyst"),
            },
            turns,
            grades,
        }
    }

    fn ungraded(turn_id: &str, pass: u8) -> GradeRecord {
        GradeRecord {
            outcome: GradeOutcome::Ungraded {
                reason: "the judge wrote nothing".into(),
            },
            ..graded(turn_id, pass, Verdict::Correct)
        }
    }

    fn costing(mut turn: TurnRecord, cost_usd: f64, rounds: u32, wall_ms: u64) -> TurnRecord {
        turn.metrics = Some(TurnMetrics {
            cost_usd,
            rounds,
            ..Default::default()
        });
        turn.wall_ms = wall_ms;
        turn
    }

    #[test]
    fn medians_and_shares() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(share(1, 4), Some(0.25));
        assert_eq!(share(0, 0), None);
    }

    #[test]
    fn the_sign_test_is_exact_and_two_sided() {
        assert_eq!(sign_test(0, 0), None);
        assert_eq!(sign_test(3, 3), Some(1.0));
        let close = |p: Option<f64>, to: f64| (p.unwrap() - to).abs() < 1e-12;
        assert!(close(sign_test(5, 0), 0.0625));
        assert!(close(sign_test(0, 6), 0.03125));
        assert!(close(sign_test(8, 2), 0.109375));
        assert!(close(sign_test(2, 8), 0.109375));
        assert!(close(sign_test(9, 1), 0.021484375));
    }

    #[test]
    fn an_arms_tally_counts_every_question_once() {
        let set = EvalSet::load(&example_dir()).unwrap();
        let [arm, _] = arms();
        let mut failed = turn(&arm, &set, "inv-001", 1);
        failed.status = TurnStatus::Failed;
        failed.answer.clear();
        let turns = vec![
            costing(turn(&arm, &set, "db-001", 1), 0.2, 4, 10_000),
            costing(turn(&arm, &set, "code-001", 1), 0.4, 8, 30_000),
            failed,
        ];
        let grades = vec![
            graded("analyst-db-001-1", 1, Verdict::Correct),
            graded("analyst-db-001-1", 2, Verdict::Partly),
            ungraded("analyst-code-001-1", 1),
            graded("analyst-code-001-1", 2, Verdict::Correct),
        ];
        let full = report(&results(turns, grades), &set);
        let all = &full.scores.arms[0][ALL];
        assert_eq!(
            (
                all.questions,
                all.asked,
                all.correct,
                all.ungraded,
                all.failed
            ),
            (3, 3, 1, 1, 1)
        );
        assert_eq!(all.correct_share, share(1, 3));
        assert_eq!(all.ungraded_share, share(1, 3));
        assert_eq!(all.failed_share, share(1, 3));
        assert_eq!(all.cited_share, Some(1.0));
        assert_eq!(all.fell_for_trap_share, Some(0.0));
        assert!((all.median_cost_usd.unwrap() - 0.3).abs() < 1e-9);
        assert_eq!(all.median_rounds, Some(6.0));
        assert_eq!(all.median_seconds, Some(20.0));
        assert_eq!(
            (
                all.judge_agreement,
                all.judge_agreement_of,
                all.judge_disagreed
            ),
            (Some(0.0), 1, 1)
        );
        assert_eq!((all.capped, all.configs), (0, 1));
        let database = &full.scores.arms[0]["database"];
        assert_eq!((database.questions, database.correct), (1, 1));
        let untouched = &full.scores.arms[1][ALL];
        assert_eq!((untouched.asked, untouched.correct_share), (0, None));
        assert!(
            (full.scores.turns_cost_usd - 0.7).abs() < 1e-9,
            "the failed turn's too"
        );
        assert!((full.scores.grading_cost_usd - 0.04).abs() < 1e-9);
    }

    #[test]
    fn an_ungraded_answer_is_never_counted_correct() {
        let set = EvalSet::load(&example_dir()).unwrap();
        let [arm, _] = arms();
        let turns = vec![turn(&arm, &set, "db-001", 1)];
        for grades in [vec![], vec![ungraded("analyst-db-001-1", 1)]] {
            let full = report(&results(turns.clone(), grades), &set);
            let all = &full.scores.arms[0][ALL];
            assert_eq!((all.correct, all.ungraded), (0, 1));
        }
    }

    #[test]
    fn arms_are_compared_question_by_question() {
        let set = EvalSet::load(&example_dir()).unwrap();
        let [baseline, candidate] = arms();
        let turns = vec![
            costing(turn(&baseline, &set, "db-001", 1), 0.2, 4, 10_000),
            costing(turn(&candidate, &set, "db-001", 1), 0.1, 2, 4_000),
            turn(&baseline, &set, "code-001", 1),
            turn(&candidate, &set, "code-001", 1),
            turn(&baseline, &set, "inv-001", 1),
        ];
        let grades = vec![
            graded("analyst-db-001-1", 1, Verdict::Wrong),
            graded("candidate-db-001-1", 1, Verdict::Partly),
            graded("analyst-code-001-1", 1, Verdict::Correct),
            graded("candidate-code-001-1", 1, Verdict::Correct),
            graded("analyst-inv-001-1", 1, Verdict::Correct),
        ];
        let full = report(&results(turns, grades), &set);
        let [comparison] = &full.scores.comparisons[..] else {
            panic!("one comparison");
        };
        let all = &comparison[ALL];
        assert_eq!(
            (all.pairs, all.better, all.worse, all.same, all.left_out),
            (2, 1, 0, 1, 1)
        );
        assert_eq!(all.p_value, Some(1.0));
        assert!(!all.distinguishable);
        assert!(all.finding().contains("not distinguishable"));
        assert!(all.finding().contains("Fewer than 6"));
        let database = &comparison["database"];
        assert!((database.median_cost_usd_diff.unwrap() + 0.1).abs() < 1e-9);
        assert_eq!(database.median_rounds_diff, Some(-2.0));
        assert_eq!(database.median_seconds_diff, Some(-6.0));
        assert_eq!(comparison["investigation"].p_value, None);
        let shown = full.render();
        assert!(shown.contains("candidate against analyst"), "{shown}");
    }

    #[test]
    fn a_consistent_difference_over_enough_questions_is_distinguishable() {
        let paired = |better, worse| {
            let grades = |verdict| {
                GradeOutcome::Graded(Grade {
                    verdict,
                    cited: false,
                    met: vec![],
                    fell_for_trap: false,
                    reason: String::new(),
                })
            };
            let question = EvalSet::load(&example_dir()).unwrap().questions[0].clone();
            let reading = |verdict| GradeRecord {
                turn_id: "t".into(),
                pass: 1,
                outcome: grades(verdict),
                model: None,
                cost_usd: 0.0,
                at: Utc::now(),
            };
            let (low, high) = (reading(Verdict::Wrong), reading(Verdict::Correct));
            let row = |read| Row {
                question: &question,
                turn: None,
                readings: vec![Some(read)],
            };
            let base: Vec<Row<'_>> = (0..better)
                .map(|_| row(&low))
                .chain((0..worse).map(|_| row(&high)))
                .collect();
            let arm: Vec<Row<'_>> = (0..better)
                .map(|_| row(&high))
                .chain((0..worse).map(|_| row(&low)))
                .collect();
            let paired = Paired::of(
                &base.iter().collect::<Vec<_>>(),
                &arm.iter().collect::<Vec<_>>(),
            );
            (paired.distinguishable, paired.finding())
        };
        let (clear, finding) = paired(9, 1);
        assert!(clear && finding.contains(": distinguishable"), "{finding}");
        assert!(!finding.contains("Fewer than"));
        assert!(!paired(6, 4).0);
        assert!(paired(1, 9).0);
    }

    #[test]
    fn a_difference_resting_on_grades_the_judge_would_not_repeat_is_not_called_one() {
        let set = EvalSet::load(&example_dir()).unwrap();
        let question = &set.questions[0];
        let reading = |pass, verdict| graded("t", pass, verdict);
        let (low, high) = (reading(1, Verdict::Wrong), reading(1, Verdict::Correct));
        let (again_low, again_high) = (reading(2, Verdict::Wrong), reading(2, Verdict::Correct));
        let row = |first, second| Row {
            question,
            turn: None,
            readings: vec![Some(first), Some(second)],
        };
        let base: Vec<Row<'_>> = (0..8).map(|_| row(&low, &again_low)).collect();
        let steady: Vec<Row<'_>> = (0..8).map(|_| row(&high, &again_high)).collect();
        let wavering: Vec<Row<'_>> = (0..8)
            .map(|n| match n < 4 {
                true => row(&high, &again_low),
                false => row(&high, &again_high),
            })
            .collect();
        let against = |arm: &[Row<'_>]| {
            Paired::of(
                &base.iter().collect::<Vec<_>>(),
                &arm.iter().collect::<Vec<_>>(),
            )
        };
        let clear = against(&steady);
        assert_eq!((clear.better, clear.unstable), (8, 0));
        assert!(clear.distinguishable);
        let shaky = against(&wavering);
        assert_eq!((shaky.better, shaky.unstable), (8, 4));
        assert!(shaky.p_value.unwrap() < SIGNIFICANCE);
        assert_eq!(shaky.p_value_stable, sign_test(4, 0));
        assert!(!shaky.distinguishable);
        assert!(
            shaky.finding().contains("4 of the pairs"),
            "{}",
            shaky.finding()
        );
    }

    #[test]
    fn capped_answers_and_mixed_configs_are_counted() {
        let set = EvalSet::load(&example_dir()).unwrap();
        let [arm, _] = arms();
        let mut capped = turn(&arm, &set, "db-001", 1);
        capped.capped = true;
        capped.config = Some("before".into());
        let mut later = turn(&arm, &set, "code-001", 1);
        later.config = Some("after".into());
        let full = report(&results(vec![capped, later], vec![]), &set);
        let all = &full.scores.arms[0][ALL];
        assert_eq!((all.capped, all.capped_share), (1, share(1, 2)));
        assert_eq!(all.configs, 2);
        assert!(full.render().contains("2 different configs"));
    }

    #[test]
    fn scores_carry_nothing_a_question_or_answer_said() {
        let set = EvalSet::load(&example_dir()).unwrap();
        let [baseline, candidate] = arms();
        let mut answered = turn(&baseline, &set, "db-001", 1);
        answered.answer = "SECRET-ANSWER about the orders table".into();
        let mut failed = turn(&candidate, &set, "db-001", 1);
        failed.status = TurnStatus::Failed;
        failed.error = Some("SECRET-ERROR".into());
        let mut reading = graded("analyst-db-001-1", 1, Verdict::Correct);
        if let GradeOutcome::Graded(grade) = &mut reading.outcome {
            grade.reason = "SECRET-REASON".into();
        }
        let full = report(&results(vec![answered, failed], vec![reading]), &set);
        let shown = serde_json::to_string(&full.scores).unwrap() + &full.render();
        for private in ["SECRET", "db-001", "shipped", "orders"] {
            assert!(!shown.contains(private), "scores hold {private:?}");
        }
        let scores = serde_json::to_string(&full.scores).unwrap();
        for name in ["analyst", "candidate", "example"] {
            assert!(!scores.contains(name), "scores name {name:?}");
        }
        fn texts(value: &serde_json::Value) -> Vec<String> {
            match value {
                serde_json::Value::String(text) => vec![text.clone()],
                serde_json::Value::Array(items) => items.iter().flat_map(texts).collect(),
                serde_json::Value::Object(fields) => fields.values().flat_map(texts).collect(),
                _ => Vec::new(),
            }
        }
        let held = texts(&serde_json::to_value(&full.scores).unwrap());
        assert!(held.is_empty(), "scores hold text: {held:?}");
        let everything = serde_json::to_string(&full).unwrap();
        for kept in ["SECRET-ANSWER", "SECRET-ERROR", "SECRET-REASON", "db-001"] {
            assert!(everything.contains(kept), "the full report lacks {kept:?}");
        }
    }

    #[test]
    fn turns_of_an_older_question_are_counted_as_left_out() {
        let set = EvalSet::load(&example_dir()).unwrap();
        let [arm, _] = arms();
        let mut old = turn(&arm, &set, "db-001", 1);
        old.question_hash = "older".into();
        let full = report(&results(vec![old], vec![]), &set);
        assert_eq!(full.scores.stale_turns, 1);
        assert_eq!(full.scores.arms[0][ALL].asked, 0);
        assert!(full.render().contains("1 recorded turns were left out"));
    }
}
