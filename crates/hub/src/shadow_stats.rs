//! The exact bound the self@1 gate is judged by (DW-676, PR 9c review).
//!
//! The pairs are an item each both stores answered: `b` found first by
//! Weaviate alone, `c` by vellev alone, `m = b + c` the discordant, of `n`.
//! The difference in self@1 is d = (c - b) / n = (2 pi - 1) q, with
//! q = m / n the discordant rate and pi = c / m vellev's share of it.
//!
//! The lower bound is taken conditionally on the discordant pairs, by
//! Clopper-Pearson: pi's lower 97.5 % bound from c of m, and q's upper
//! 97.5 % bound from m of n, together a one-sided 95 % bound (Bonferroni).
//! It is exact, never the Wald bound's normal approximation, which is
//! anti-conservative at the handful of discordant pairs these runs give.

/// ln Γ(x), x > 0 (Lanczos, g = 7, nine coefficients: ample for counts
/// of a few thousand).
pub fn ln_gamma(x: f64) -> f64 {
    const G: f64 = 7.0;
    const C: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        let pi = std::f64::consts::PI;
        return (pi / (pi * x).sin()).ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let mut a = C[0];
    let t = x + G + 0.5;
    for (i, c) in C.iter().enumerate().skip(1) {
        a += c / (x + i as f64);
    }
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + a.ln()
}

fn ln_choose(n: u64, k: u64) -> f64 {
    ln_gamma(n as f64 + 1.0) - ln_gamma(k as f64 + 1.0) - ln_gamma((n - k) as f64 + 1.0)
}

/// P(X in `ks`) for X ~ Binomial(n, p), 0 < p < 1.
fn binomial_sum(n: u64, p: f64, ks: impl Iterator<Item = u64>) -> f64 {
    let (lp, lq) = (p.ln(), (1.0 - p).ln());
    ks.map(|k| (ln_choose(n, k) + k as f64 * lp + (n - k) as f64 * lq).exp())
        .sum()
}

/// The p in (0, 1) at which `tail(p)` crosses `level`, `tail` rising with
/// p (bisection to well under 1e-12).
fn solve(level: f64, tail: impl Fn(f64) -> f64) -> f64 {
    let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if tail(mid) < level {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// Clopper-Pearson upper bound of a rate from `x` of `n`, at `alpha` (one
/// side): 1 when x = n.
pub fn cp_upper(x: u64, n: u64, alpha: f64) -> f64 {
    if x >= n {
        return 1.0;
    }
    // P(X <= x) falls as p rises: its complement rises.
    solve(1.0 - alpha, |p| 1.0 - binomial_sum(n, p, 0..=x))
}

/// Clopper-Pearson lower bound of a rate from `x` of `n`, at `alpha` (one
/// side): 0 when x = 0.
pub fn cp_lower(x: u64, n: u64, alpha: f64) -> f64 {
    if x == 0 {
        return 0.0;
    }
    solve(alpha, |p| binomial_sum(n, p, x..=n))
}

/// The one-sided 95 % lower bound of vellev's self@1 less Weaviate's,
/// from `b` and `c` of `n` pairs: min((2 pi_L - 1) q_U, d), each piece at
/// 97.5 %.
pub fn self_first_lower(b: u64, c: u64, n: u64) -> f64 {
    let m = b + c;
    let q_upper = cp_upper(m, n, 0.025);
    let pi_lower = cp_lower(c, m, 0.025);
    let d = (c as f64 - b as f64) / n as f64;
    ((2.0 * pi_lower - 1.0) * q_upper).min(d)
}

/// Whether `n` pairs can show non-inferiority at `margin` at all: with no
/// discordant pair, the best that can be seen.
pub fn can_show(n: u64, margin: f64) -> bool {
    n > 0 && self_first_lower(0, 0, n) > -margin
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against the reviewer's scipy values (rev441/stats.py, beta.ppf).
    #[test]
    fn the_bound_matches_the_reviewer_s_exact_values() {
        for (b, c, n, want) in [
            (0, 0, 300, -0.012_220_974_694_293_55),
            (1, 1, 1000, -0.007_024_552_728_610_332),
            (3, 6, 1000, -0.006_830_303_569_221_727),
            (10, 15, 1000, -0.008_316_192_866_743_923),
            (10, 10, 3000, -0.004_687_376_575_913_772),
            (0, 0, 1000, -0.003_682_083_896_865_671),
            (30, 40, 7000, -0.001_325_187_094_952_608_3),
            (0, 0, 600, -0.006_129_271_330_669_258),
        ] {
            let got = self_first_lower(b, c, n);
            assert!((got - want).abs() < 1e-9, "{b} {c} {n}: {got} vs {want}");
        }
    }

    /// 736 pairs are the fewest that can show half a point.
    #[test]
    fn the_fewest_pairs_that_can_show_the_margin() {
        assert!(!can_show(735, 0.005));
        assert!(can_show(736, 0.005));
        assert!(!can_show(0, 0.005));
    }

    #[test]
    fn ln_gamma_is_exact_enough() {
        assert!((ln_gamma(1.0)).abs() < 1e-12);
        assert!((ln_gamma(11.0) - 3_628_800f64.ln()).abs() < 1e-10);
    }
}
