//! The arithmetic of a comparison of code search: the keys every random
//! choice is made by, the sample's allocation, a text's first sentence, the
//! bootstrap and its interval, recall, the same-top rule, a node's work, and
//! the gates. Every function here is pure.

use std::collections::BTreeMap;
use std::ops::Add;

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

use super::{Agreement, Answer, Check1, Check2, Gate, Prefix, Scored};

const ABBREVIATIONS: [&str; 5] = ["e.g", "i.e", "etc", "vs", "cf"];
const OVERALL_FLOOR: f64 = -0.02;
const KIND_FLOOR: f64 = -0.05;
const GATED_KIND: usize = 300;
const RECALL_FLOOR: f64 = 0.97;
pub(super) const SAME_DISTANCE: f32 = 1e-6;

pub(super) fn key(seed: u64, purpose: &str, parts: &[&str]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&seed.to_le_bytes());
    for part in std::iter::once(&purpose).chain(parts) {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.finalize().into()
}

pub(super) fn allocated(counts: &BTreeMap<String, usize>, n: usize) -> BTreeMap<String, usize> {
    let total = counts.values().sum::<usize>() as u128;
    if n as u128 >= total {
        return counts.clone();
    }
    let share = |count: usize| (n as u128 * count as u128 / total) as usize;
    let floors: BTreeMap<String, usize> = counts
        .iter()
        .map(|(kind, count)| (kind.clone(), share(*count)))
        .collect();
    let left = n - floors.values().sum::<usize>();
    let mut by_remainder: Vec<(&String, u128)> = counts
        .iter()
        .map(|(kind, count)| (kind, n as u128 * *count as u128 % total))
        .collect();
    by_remainder.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    by_remainder
        .into_iter()
        .take(left)
        .fold(floors, |mut allocated, (kind, _)| {
            *allocated.entry(kind.clone()).or_default() += 1;
            allocated
        })
}

pub(super) fn first_sentence(text: &str) -> Option<String> {
    let joined = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let ends = |at: usize| {
        let word = joined[..at].rsplit(' ').next().unwrap_or_default();
        word.chars().count() > 1 && !ABBREVIATIONS.contains(&word.to_lowercase().as_str())
    };
    let cut = joined
        .match_indices(". ")
        .map(|(at, _)| at)
        .find(|at| ends(*at))
        .map_or(joined.as_str(), |at| &joined[..=at]);
    Some(cut.trim().to_string()).filter(|sentence| !sentence.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Form {
    Name,
    Doc,
    Summary,
}

impl Form {
    pub(super) fn weight(self) -> f64 {
        match self {
            Form::Name | Form::Doc => 1.0,
            Form::Summary => 2.0,
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Form::Name => "name",
            Form::Doc => "doc",
            Form::Summary => "summary",
        }
    }
}

pub(super) fn stratum_weight(population: usize, sampled: usize) -> f64 {
    population as f64 / sampled.max(1) as f64
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct Sum {
    pub(super) weight: f64,
    pub(super) d: f64,
    pub(super) d_prefix: f64,
    pub(super) d_raw: f64,
    pub(super) weaviate: f64,
    pub(super) vellev: f64,
}

impl Sum {
    pub(super) fn mean(&self, measure: fn(&Sum) -> f64) -> f64 {
        match self.weight == 0.0 {
            true => 0.0,
            false => measure(self) / self.weight,
        }
    }
}

impl Add for Sum {
    type Output = Sum;

    fn add(self, other: Sum) -> Sum {
        Sum {
            weight: self.weight + other.weight,
            d: self.d + other.d,
            d_prefix: self.d_prefix + other.d_prefix,
            d_raw: self.d_raw + other.d_raw,
            weaviate: self.weaviate + other.weaviate,
            vellev: self.vellev + other.vellev,
        }
    }
}

impl std::iter::Sum for Sum {
    fn sum<I: Iterator<Item = Sum>>(sums: I) -> Sum {
        sums.fold(Sum::default(), Add::add)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Unit {
    pub(super) stratum: String,
    pub(super) queries: usize,
    pub(super) sum: Sum,
}

pub(super) fn strata(units: &[Unit]) -> BTreeMap<&str, Vec<&Unit>> {
    units.iter().fold(BTreeMap::new(), |mut strata, unit| {
        strata
            .entry(unit.stratum.as_str())
            .or_insert_with(Vec::new)
            .push(unit);
        strata
    })
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Replicates {
    pub(super) overall: Vec<Sum>,
    pub(super) strata: BTreeMap<String, Vec<Sum>>,
}

pub(super) fn bootstrap(units: &[Unit], resamples: usize, seed: u64) -> Replicates {
    let strata = strata(units);
    let mut rng = ChaCha8Rng::from_seed(key(seed, "bootstrap", &[]));
    (0..resamples).fold(Replicates::default(), |mut replicates, _| {
        let drawn: Vec<(&str, Sum)> = strata
            .iter()
            .map(|(stratum, units)| {
                let sum = (0..units.len())
                    .map(|_| units[rng.gen_range(0..units.len())].sum)
                    .sum::<Sum>();
                (*stratum, sum)
            })
            .collect();
        let all = drawn.iter().map(|(_, sum)| *sum).sum::<Sum>();
        replicates.overall.push(all);
        for (stratum, sum) in drawn {
            replicates
                .strata
                .entry(stratum.to_string())
                .or_default()
                .push(sum);
        }
        replicates
    })
}

pub(super) fn interval(replicates: &mut [f64]) -> (f64, f64) {
    replicates.sort_by(f64::total_cmp);
    let count = replicates.len();
    if count == 0 {
        return (0.0, 0.0);
    }
    let upper = (count * 975).div_ceil(1000).saturating_sub(1);
    (replicates[count * 25 / 1000], replicates[upper])
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub(super) struct Estimate {
    pub(super) d: f64,
    pub(super) lower: f64,
    pub(super) upper: f64,
}

impl Estimate {
    pub(super) fn of(total: Sum, replicates: &[Sum], measure: fn(&Sum) -> f64) -> Self {
        let mut means: Vec<f64> = replicates.iter().map(|sum| sum.mean(measure)).collect();
        let (lower, upper) = interval(&mut means);
        Self {
            d: total.mean(measure),
            lower,
            upper,
        }
    }
}

fn distance(answer: &Answer) -> f32 {
    answer.distance.unwrap_or_default()
}

pub(super) fn recall(index: &[Answer], exact: &[Answer], k: usize) -> f64 {
    let k = k.min(exact.len());
    let Some(boundary) = k.checked_sub(1).map(|last| distance(&exact[last])) else {
        return 1.0;
    };
    let found = index
        .iter()
        .take(k)
        .filter(|hit| distance(hit) <= boundary + SAME_DISTANCE)
        .count();
    found as f64 / k as f64
}

pub(super) fn first_agrees(first: Option<&Answer>, scanned: Option<f32>) -> bool {
    first.is_none_or(|first| {
        scanned.is_some_and(|scanned| (scanned - distance(first)).abs() <= SAME_DISTANCE)
    })
}

pub(super) fn same_top(reference: &[Answer], other: &[Answer], depth: usize, within: f32) -> bool {
    let runs: Vec<usize> = reference
        .iter()
        .scan((0usize, None::<f32>), |(run, last), hit| {
            if last.is_some_and(|last| distance(hit) - last > within) {
                *run += 1;
            }
            *last = Some(distance(hit));
            Some(*run)
        })
        .collect();
    let run_of = |object: &Option<String>| {
        reference
            .iter()
            .position(|hit| hit.object == *object)
            .map(|at| runs[at])
    };
    reference.len().min(depth) == other.len().min(depth)
        && other
            .iter()
            .zip(&runs)
            .take(depth)
            .all(|(hit, run)| run_of(&hit.object) == Some(*run))
}

pub(super) fn work_sums(text: &str, route: &str) -> Scored {
    let route = format!("route=\"{route}\"");
    let of = |series: &str, kinds: &[&str]| {
        let kinds: Vec<String> = kinds
            .iter()
            .map(|kind| format!("kind=\"{kind}\""))
            .collect();
        text.lines()
            .filter(|line| line.starts_with(series) && line.contains(&route))
            .filter(|line| kinds.iter().any(|kind| line.contains(kind)))
            .filter_map(|line| line.rsplit_once(' ')?.1.parse::<f64>().ok())
            .sum::<f64>()
            .round() as u64
    };
    Scored {
        searches: of("vellev_search_work_count{", &["estimated"]),
        exactly: of(
            "vellev_search_work_sum{",
            &["exact_segments", "exact_memtable"],
        ),
        estimated: of("vellev_search_work_sum{", &["estimated"]),
    }
}

pub(super) fn shortlisted(before: Scored, after: Scored, held: usize) -> Option<bool> {
    (after.searches == before.searches + 1).then(|| {
        after.estimated > before.estimated
            || after.exactly.saturating_sub(before.exactly) < held as u64
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Applying {
    Applied,
    NotApplied,
    ByStatement,
}

pub(super) fn gates(
    check1: &Check1,
    prefix: &Prefix,
    agreement: &Agreement,
    check2: &Check2,
) -> Vec<Gate> {
    let gate = |name: &str, passed: bool, said: String| Gate {
        name: name.to_string(),
        passed,
        said,
    };
    let lower = check1.overall.lower;
    let kinds = check1
        .kinds
        .iter()
        .filter(|(_, (queries, _))| *queries >= GATED_KIND)
        .map(|(kind, (queries, estimate))| {
            let said = format!(
                "over {queries} queries the lower bound of D is {:.4}, and {KIND_FLOOR} is needed",
                estimate.lower
            );
            gate(&format!("kind {kind}"), estimate.lower >= KIND_FLOOR, said)
        });
    let applied = match prefix.applied {
        Applying::Applied => "the node's text answers are made with its prefix",
        Applying::NotApplied => "no text answer shows the node's prefix",
        Applying::ByStatement => "not applied, by the operator's statement",
    };
    let recall = match check2.recall {
        Some(recall) => format!("over {} shortlisted queries the mean recall at 10 is {recall:.4}, and {RECALL_FLOOR} is needed", check2.shortlisted),
        None => "exhaustive: the node shortlisted no query".to_string(),
    };
    let agreed = format!(
        "{} of {} same queries' vector answers are their text answers",
        agreement.same_agreed, agreement.same
    );
    let asked: usize = check1.kinds.values().map(|(queries, _)| queries).sum();
    let overall = match asked {
        0 => gate("overall", false, "no query was asked".to_string()),
        _ => gate(
            "overall",
            lower >= OVERALL_FLOOR,
            format!("the lower bound of D is {lower:.4}, and {OVERALL_FLOOR} is needed"),
        ),
    };
    std::iter::once(overall)
        .chain(kinds)
        .chain([
            gate("agreement", agreement.same_agreed == agreement.same, agreed),
            gate(
                "applied",
                prefix.applied != Applying::NotApplied,
                applied.to_string(),
            ),
            gate(
                "recall",
                check2.recall.is_none_or(|recall| recall >= RECALL_FLOOR),
                recall,
            ),
            gate(
                "self_check",
                check2.self_checked,
                format!("the scan agrees with the node: {}", check2.self_checked),
            ),
        ])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::VectorSearchResult;
    use super::*;

    fn of(weight: f64, d: f64) -> Sum {
        Sum {
            weight,
            d: weight * d,
            ..Sum::default()
        }
    }

    /// One unit of a kind per difference, each of weight 1.
    fn units_of(stratum: &str, differences: &[f64]) -> Vec<Unit> {
        differences
            .iter()
            .map(|d| Unit {
                stratum: stratum.to_string(),
                queries: 1,
                sum: of(1.0, *d),
            })
            .collect()
    }

    fn means(replicates: &[Sum]) -> Vec<f64> {
        replicates.iter().map(|sum| sum.mean(|sum| sum.d)).collect()
    }

    fn ci(units: &[Unit], seed: u64) -> (f64, f64) {
        interval(&mut means(&bootstrap(units, 200, seed).overall))
    }

    /// A constant difference has a degenerate interval, and each replicate
    /// is the mean of one draw of as many units as each kind has.
    #[test]
    fn the_ci_of_a_constant_difference_is_that_constant() {
        for seed in [1, 7, 99] {
            assert_eq!(
                ci(&units_of("Function", &[-0.25; 12]), seed),
                (-0.25, -0.25)
            );
            assert_eq!(ci(&units_of("Function", &[0.0; 12]), seed), (0.0, 0.0));
        }
        let units = [units_of("A", &[0.0, -1.0]), units_of("B", &[-1.0; 2])].concat();
        let replicates = bootstrap(&units, 200, 3).overall;
        assert!(
            replicates.iter().all(|sum| sum.weight == 4.0),
            "{replicates:?}"
        );
        let drawn = means(&replicates);
        assert!(
            drawn.iter().all(|d| [-0.5, -0.75, -1.0].contains(d)),
            "{drawn:?}"
        );
    }

    #[test]
    fn resampling_keeps_each_kinds_count() {
        let units = [units_of("A", &[0.0]), units_of("B", &[-1.0])].concat();
        let replicates = bootstrap(&units, 100, 5);
        assert!(means(&replicates.overall).iter().all(|d| *d == -0.5));
        assert_eq!(means(&replicates.strata["A"]), vec![0.0; 100]);
    }

    #[test]
    fn summaries_count_twice() {
        let (names, summaries, losing) = (100, 100, 3);
        let d = (0..names)
            .map(|_| of(Form::Name.weight(), 0.0))
            .chain((0..summaries).map(|at| {
                let d = if at < losing { -1.0 } else { 0.0 };
                of(Form::Summary.weight(), d)
            }))
            .sum::<Sum>()
            .mean(|sum| sum.d);
        let weighted = -(2.0 * losing as f64) / (names as f64 + 2.0 * summaries as f64);
        let unweighted = -(losing as f64) / (names + summaries) as f64;
        assert!((d - weighted).abs() < 1e-12, "{d}");
        assert!((weighted - -0.02).abs() < 1e-12 && (unweighted - -0.015).abs() < 1e-12);
    }

    #[test]
    fn stratum_weights_correct_what_rounding_allocates() {
        let population: BTreeMap<String, usize> = [("a", 8), ("b", 5), ("c", 2)]
            .map(|(k, n)| (k.to_string(), n))
            .into();
        let sampled = allocated(&population, 4);
        assert_eq!(sampled.values().copied().collect::<Vec<_>>(), [2, 1, 1]);
        let weights: Vec<f64> = population
            .iter()
            .map(|(kind, n)| Form::Name.weight() * stratum_weight(*n, sampled[kind]))
            .collect();
        assert_eq!(weights, [4.0, 5.0, 2.0]);
        let means = [("a", [0.0, -1.0]), ("b", [-1.0, 0.0]), ("c", [1.0, 0.0])];
        let units = means
            .iter()
            .flat_map(|(kind, ds)| {
                let w = Form::Name.weight() * stratum_weight(population[*kind], sampled[*kind]);
                ds.iter().take(sampled[*kind]).map(move |d| of(w, *d))
            })
            .sum::<Sum>();
        let d = units.mean(|sum| sum.d);
        let kind_mean = |kind: &str| {
            let (_, ds) = means.iter().find(|(k, _)| *k == kind).unwrap();
            ds[..sampled[kind]].iter().sum::<f64>() / sampled[kind] as f64
        };
        let population_weighted = population
            .iter()
            .map(|(kind, n)| *n as f64 * kind_mean(kind))
            .sum::<f64>()
            / population.values().sum::<usize>() as f64;
        assert!(
            (d - population_weighted).abs() < 1e-12,
            "{d} {population_weighted}"
        );
    }

    #[test]
    fn allocation_is_in_integers_with_ties_by_name() {
        let counts = |pairs: &[(&str, usize)]| -> BTreeMap<String, usize> {
            pairs.iter().map(|(k, n)| (k.to_string(), *n)).collect()
        };
        let tied = allocated(&counts(&[("a", 7), ("b", 7), ("c", 1)]), 5);
        assert_eq!(tied.values().copied().collect::<Vec<_>>(), [3, 2, 0]);
        for (seed, n) in (0..40u64).zip((1..).step_by(7)) {
            let generated: BTreeMap<String, usize> = (0..5)
                .map(|at| {
                    let k = key(seed, "counts", &[&at.to_string()]);
                    (format!("k{at}"), usize::from(k[0]) % 50 + 1)
                })
                .collect();
            let total: usize = generated.values().sum();
            let taken = allocated(&generated, n);
            match n >= total {
                true => assert_eq!(taken, generated),
                false => {
                    assert_eq!(taken.values().sum::<usize>(), n);
                    assert!(taken.iter().all(|(kind, n)| *n <= generated[kind]));
                }
            }
        }
    }

    #[test]
    fn the_interval_takes_the_pinned_percentiles() {
        let mut sorted: Vec<f64> = (0..10_000).map(f64::from).collect();
        assert_eq!(interval(&mut sorted), (250.0, 9_749.0));
        assert_eq!(interval(&mut [3.5]), (3.5, 3.5));
    }

    fn check1_with_kind(queries: usize, lower: f64) -> Check1 {
        let estimate = Estimate {
            lower,
            ..Estimate::default()
        };
        Check1 {
            kinds: [("Function".to_string(), (queries, estimate))].into(),
            ..Check1::default()
        }
    }

    fn failed(check1: &Check1) -> Vec<String> {
        let prefix = Prefix {
            query_prefix: None,
            applied: Applying::Applied,
            d_prefix: None,
        };
        let check2 = Check2 {
            self_checked: true,
            ..Check2::default()
        };
        gates(check1, &prefix, &Agreement::default(), &check2)
            .into_iter()
            .filter(|gate| !gate.passed)
            .map(|gate| gate.name)
            .collect()
    }

    #[test]
    fn a_kind_under_300_queries_is_reported_not_gated() {
        assert!(failed(&check1_with_kind(299, -0.5)).is_empty());
        assert_eq!(failed(&check1_with_kind(300, -0.5)), ["kind Function"]);
    }

    #[test]
    fn the_overall_gate_is_at_minus_two_points() {
        let at = |lower: f64| Check1 {
            overall: Estimate {
                lower,
                ..Estimate::default()
            },
            ..check1_with_kind(10, 0.0)
        };
        assert!(failed(&at(-0.0199)).is_empty());
        assert!(failed(&at(-0.02)).is_empty());
        assert_eq!(failed(&at(-0.0201)), ["overall"]);
        assert_eq!(failed(&Check1::default()), ["overall"]);
    }

    #[test]
    fn the_same_seed_gives_the_same_replicates() {
        let units = [
            units_of("A", &[0.0, -1.0, 1.0, 0.0]),
            units_of("B", &[-1.0, 0.0, 0.0]),
        ]
        .concat();
        assert_eq!(bootstrap(&units, 50, 11), bootstrap(&units, 50, 11));
        assert_ne!(bootstrap(&units, 50, 11), bootstrap(&units, 50, 12));
    }

    fn hit(object: &str, distance: f32) -> Answer {
        Answer {
            object: Some(object.to_string()),
            distance: Some(distance),
            found: VectorSearchResult {
                entity_id: object.to_string(),
                repo_id: "r".to_string(),
                qualified_name: object.to_string(),
                kind: "Function".to_string(),
                file_path: "src/lib.rs".to_string(),
                start_line: 1,
                end_line: 2,
                score: 0.5,
            },
        }
    }

    fn placed(pairs: &[(&str, f32)]) -> Vec<Answer> {
        pairs
            .iter()
            .map(|(object, distance)| hit(object, *distance))
            .collect()
    }

    #[test]
    fn recall_counts_a_boundary_tie_as_found() {
        let exact: Vec<(String, f32)> = (0..10)
            .map(|n| (format!("e{n}"), n as f32 * 0.01))
            .collect();
        let exact = placed(
            &exact
                .iter()
                .map(|(o, d)| (o.as_str(), *d))
                .collect::<Vec<_>>(),
        );
        let mut tied = exact.clone();
        tied[9].object = Some("other".to_string());
        assert_eq!(recall(&tied, &exact, 10), 1.0);
        let mut above = exact.clone();
        above[9] = hit("other", exact[9].distance.unwrap() + 5e-7);
        assert_eq!(recall(&above, &exact, 10), 1.0);
        let mut missing = exact.clone();
        missing[9] = hit("far", 1.0);
        assert!((recall(&missing, &exact, 10) - 0.9).abs() < 1e-12);
        let held = 4;
        let k = held.min(10);
        assert_eq!(recall(&exact[..4], &exact[..4], k), 1.0);
        assert!((recall(&missing[6..], &exact[..4], k) - 0.0).abs() < 1e-12);
    }

    /// The scan's own distance of the index's first hit must be the node's,
    /// within 1e-6; a first hit the scan never listed fails.
    #[test]
    fn the_self_check_holds_the_scan_to_the_nodes_distances() {
        let first = hit("a", 0.1);
        assert!(first_agrees(Some(&first), Some(0.1 + 5e-7)));
        assert!(!first_agrees(Some(&first), Some(0.1 + 1e-5)));
        assert!(!first_agrees(Some(&first), None));
        assert!(first_agrees(None, None));
    }

    #[test]
    fn same_top_tolerates_near_ties_only() {
        let reference: Vec<Answer> = (0..20)
            .map(|n| hit(&format!("o{n}"), n as f32 * 0.01))
            .collect();
        assert!(same_top(&reference, &reference, 10, 1e-3));
        let near = |gap: f32| {
            let mut reference = reference.clone();
            reference[4].distance = reference[3].distance.map(|distance| distance + gap);
            let mut other = reference.clone();
            other.swap(3, 4);
            (reference, other)
        };
        let (close, swapped) = near(5e-4);
        assert!(same_top(&close, &swapped, 10, 1e-3));
        let (apart, swapped) = near(2e-3);
        assert!(!same_top(&apart, &swapped, 10, 1e-3));
        let mut boundary = reference.clone();
        boundary[10].distance = boundary[9].distance.map(|distance| distance + 5e-4);
        let mut other = boundary.clone();
        other.swap(9, 10);
        assert!(same_top(&boundary, &other, 10, 1e-3));
        let mut stranger = reference.clone();
        stranger[2].object = Some("absent".to_string());
        assert!(!same_top(&reference, &stranger, 10, 1e-3));
    }

    #[test]
    fn first_sentence() {
        assert_eq!(
            super::first_sentence("Reads the file\n   and parses it. Then more."),
            Some("Reads the file and parses it.".to_string())
        );
        assert_eq!(
            super::first_sentence("Parses e.g. TOML. Then"),
            Some("Parses e.g. TOML.".to_string())
        );
        assert_eq!(
            super::first_sentence("Bumps 1.5 to 2."),
            Some("Bumps 1.5 to 2.".to_string())
        );
        assert_eq!(
            super::first_sentence("no full stop"),
            Some("no full stop".to_string())
        );
        assert_eq!(super::first_sentence(" \n "), None);
    }

    #[test]
    fn keys_keep_their_parts_apart() {
        assert_ne!(key(1, "p", &["ab", "c"]), key(1, "p", &["a", "bc"]));
        assert_eq!(key(1, "p", &["ab", "c"]), key(1, "p", &["ab", "c"]));
    }

    const METRICS: &str = r#"# HELP vellev_search_work What a search did.
# TYPE vellev_search_work histogram
vellev_search_work_sum{route="/v1/tenants/{tenant}/collections/{collection}/nearest",kind="exact_memtable"} 100.0
vellev_search_work_count{route="/v1/tenants/{tenant}/collections/{collection}/nearest",kind="exact_memtable"} 3
vellev_search_work_bucket{le="10.0",route="/v1/tenants/{tenant}/collections/{collection}/nearest",kind="exact_memtable"} 1
vellev_search_work_sum{route="/v1/tenants/{tenant}/collections/{collection}/nearest",kind="exact_segments"} 2000.0
vellev_search_work_count{route="/v1/tenants/{tenant}/collections/{collection}/nearest",kind="exact_segments"} 3
vellev_search_work_sum{kind="estimated",route="/v1/tenants/{tenant}/collections/{collection}/nearest"} 1200.0
vellev_search_work_count{kind="estimated",route="/v1/tenants/{tenant}/collections/{collection}/nearest"} 3
vellev_search_work_sum{route="/v1/tenants/{tenant}/collections/{collection}/hybrid",kind="estimated"} 9000.0
vellev_search_work_count{route="/v1/tenants/{tenant}/collections/{collection}/hybrid",kind="estimated"} 7
vellev_search_work_sum{route="/v1/tenants/{tenant}/collections/{collection}/hybrid",kind="exact_segments"} 5000.0
"#;

    #[test]
    fn shortlisted_reads_the_nodes_work() {
        let route = "/v1/tenants/{tenant}/collections/{collection}/nearest";
        assert_eq!(
            work_sums(METRICS, route),
            Scored {
                searches: 3,
                exactly: 2_100,
                estimated: 1_200,
            }
        );
        let before = Scored {
            searches: 10,
            exactly: 50_000,
            estimated: 7_000,
        };
        let after = |searches: u64, exactly: u64, estimated: u64| Scored {
            searches: before.searches + searches,
            exactly: before.exactly + exactly,
            estimated: before.estimated + estimated,
        };
        assert_eq!(shortlisted(before, after(1, 0, 1_200), 5_000), Some(true));
        assert_eq!(shortlisted(before, after(1, 3_000, 0), 5_000), Some(true));
        assert_eq!(shortlisted(before, after(1, 5_000, 0), 5_000), Some(false));
        assert_eq!(shortlisted(before, after(2, 10_000, 0), 5_000), None);
    }
}
