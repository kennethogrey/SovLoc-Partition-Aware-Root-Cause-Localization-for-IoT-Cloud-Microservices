//! Statistics for the placement study: Wilson intervals on proportions,
//! Kaplan-Meier survival and the log-rank test for censored time to diagnosis,
//! bootstrap confidence intervals, the two one-sided tests (TOST) for
//! equivalence, Holm correction for multiplicity, and paired bootstrap for the
//! difference between two placements run on the same seeds.
//!
//! Everything is pure Rust with no external dependency, so the analysis compiles
//! and runs with the same pinned toolchain as the rest of the workspace and one
//! command regenerates every reported number. Randomness (the bootstrap) uses a
//! small seeded xorshift generator, so a given seed always gives the same
//! interval.
//!
//! The normal approximation is used for the tail probabilities of the log-rank
//! and TOST tests. For the sample sizes in this study (at least thirty per cell)
//! this is standard; where an exact small-sample test would differ it is noted
//! at the call site.

/// A right-censored observation: `time` is the time to the event or to the end
/// of the observation window, and `event` is true if the event (a confirmed
/// correct localization) was observed, false if the run ended first (censored).
#[derive(Clone, Copy, Debug)]
pub struct Censored {
    pub time: f64,
    pub event: bool,
}

/// A tiny deterministic xorshift64* generator, so bootstrap intervals are
/// reproducible from a seed without pulling in a dependency.
pub struct XorShift64(u64);

impl XorShift64 {
    pub fn new(seed: u64) -> Self {
        // Avoid the zero state.
        XorShift64(seed ^ 0x9E3779B97F4A7C15)
    }
    #[inline]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    /// Uniform in [0, 1).
    #[inline]
    pub fn uniform(&mut self) -> f64 {
        // 53-bit mantissa for a double in [0,1).
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Uniform integer in [0, n).
    #[inline]
    pub fn below(&mut self, n: usize) -> usize {
        (self.uniform() * n as f64) as usize % n.max(1)
    }
}

/// Standard normal cumulative distribution function via a high-accuracy `erf`.
pub fn normal_cdf(x: f64) -> f64 {
    0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
}

/// Error function (Abramowitz and Stegun 7.1.26), accurate to about 1e-7.
pub fn erf(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let y = 1.0
        - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t
            + 0.254829592)
            * t
            * (-x * x).exp();
    sign * y
}

/// Survival function of a chi-square distribution with one degree of freedom:
/// P(X > x) = 2 (1 - Phi(sqrt(x))).
pub fn chi2_1df_sf(x: f64) -> f64 {
    if x <= 0.0 {
        return 1.0;
    }
    2.0 * (1.0 - normal_cdf(x.sqrt()))
}

/// Wilson score interval for a binomial proportion at confidence level set by
/// `z` (1.96 for 95%). Returns (point, lo, hi). Correct at the extremes 0 and n,
/// unlike the normal (Wald) interval.
pub fn wilson_interval(successes: usize, n: usize, z: f64) -> (f64, f64, f64) {
    if n == 0 {
        return (f64::NAN, f64::NAN, f64::NAN);
    }
    let p = successes as f64 / n as f64;
    let n = n as f64;
    let z2 = z * z;
    let denom = 1.0 + z2 / n;
    let centre = (p + z2 / (2.0 * n)) / denom;
    let half = (z * ((p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt())) / denom;
    (p, (centre - half).max(0.0), (centre + half).min(1.0))
}

/// Mean of a slice (NaN if empty).
pub fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Percentile bootstrap confidence interval for an arbitrary statistic of a
/// sample. Returns (point estimate on the data, lo, hi) at the central
/// `1 - 2*alpha` level (alpha = 0.025 gives a 95% interval). Deterministic given
/// `seed`.
pub fn bootstrap_ci<F: Fn(&[f64]) -> f64>(
    xs: &[f64],
    stat: F,
    n_boot: usize,
    alpha: f64,
    seed: u64,
) -> (f64, f64, f64) {
    let point = stat(xs);
    if xs.len() < 2 {
        return (point, point, point);
    }
    let mut rng = XorShift64::new(seed);
    let mut boots = Vec::with_capacity(n_boot);
    let mut sample = vec![0.0; xs.len()];
    for _ in 0..n_boot {
        for s in sample.iter_mut() {
            *s = xs[rng.below(xs.len())];
        }
        boots.push(stat(&sample));
    }
    boots.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let lo = boots[((alpha) * n_boot as f64) as usize];
    let hi = boots[(((1.0 - alpha) * n_boot as f64) as usize).min(n_boot - 1)];
    (point, lo, hi)
}

/// Paired bootstrap for the mean difference `a[i] - b[i]` of two placements run
/// on the same seeds. Returns (mean diff, lo, hi).
pub fn paired_diff_ci(a: &[f64], b: &[f64], n_boot: usize, alpha: f64, seed: u64) -> (f64, f64, f64) {
    assert_eq!(a.len(), b.len());
    let diffs: Vec<f64> = a.iter().zip(b).map(|(x, y)| x - y).collect();
    bootstrap_ci(&diffs, mean, n_boot, alpha, seed)
}

/// Kaplan-Meier survival estimate. Returns the step function as (time, survival)
/// pairs at each observed event time, plus the median survival time (the first
/// time at which survival drops to 0.5 or below; NaN if survival never reaches
/// 0.5, i.e. more than half the runs are censored before the event).
pub fn kaplan_meier(data: &[Censored]) -> (Vec<(f64, f64)>, f64) {
    let mut d = data.to_vec();
    d.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));
    let n = d.len();
    let mut curve = Vec::new();
    let mut surv = 1.0;
    let mut median = f64::NAN;
    let mut i = 0;
    while i < n {
        let t = d[i].time;
        // Everyone with time >= t is still at risk at t.
        let at_risk = d[i..].len();
        // Count events exactly at t.
        let mut events = 0usize;
        let mut j = i;
        while j < n && d[j].time == t {
            if d[j].event {
                events += 1;
            }
            j += 1;
        }
        if events > 0 {
            surv *= 1.0 - events as f64 / at_risk as f64;
            curve.push((t, surv));
            if median.is_nan() && surv <= 0.5 {
                median = t;
            }
        }
        i = j;
    }
    (curve, median)
}

/// Log-rank test comparing two groups of censored observations. Returns
/// (chi-square statistic with 1 df, p-value). A large statistic / small p means
/// the two survival curves differ.
pub fn logrank(group_a: &[Censored], group_b: &[Censored]) -> (f64, f64) {
    // Merge all distinct event times.
    let mut times: Vec<f64> = group_a
        .iter()
        .chain(group_b.iter())
        .filter(|c| c.event)
        .map(|c| c.time)
        .collect();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    times.dedup();

    let at_risk = |g: &[Censored], t: f64| g.iter().filter(|c| c.time >= t).count();
    let events_at = |g: &[Censored], t: f64| g.iter().filter(|c| c.event && c.time == t).count();

    let mut o_minus_e = 0.0_f64; // observed - expected in group A
    let mut var = 0.0_f64;
    for &t in &times {
        let n1 = at_risk(group_a, t) as f64;
        let n2 = at_risk(group_b, t) as f64;
        let n = n1 + n2;
        if n < 2.0 {
            continue;
        }
        let o1 = events_at(group_a, t) as f64;
        let o2 = events_at(group_b, t) as f64;
        let o = o1 + o2;
        let e1 = o * n1 / n;
        o_minus_e += o1 - e1;
        // Hypergeometric variance of O1.
        var += o * (n1 / n) * (n2 / n) * ((n - o) / (n - 1.0));
    }
    if var <= 0.0 {
        return (0.0, 1.0);
    }
    let chi2 = o_minus_e * o_minus_e / var;
    (chi2, chi2_1df_sf(chi2))
}

/// Two one-sided tests (TOST) for the equivalence of two sample means within a
/// symmetric margin. Returns (difference a-b, p_lower, p_upper); the means are
/// declared equivalent at level `alpha` when both p-values are below `alpha`.
/// Uses Welch standard error and the normal approximation (adequate at n >= 30).
pub fn tost_means(a: &[f64], b: &[f64], margin: f64) -> (f64, f64, f64) {
    let (ma, mb) = (mean(a), mean(b));
    let va = sample_var(a);
    let vb = sample_var(b);
    let se = (va / a.len() as f64 + vb / b.len() as f64).sqrt();
    let diff = ma - mb;
    if se <= 0.0 {
        // No variance: equivalent iff the difference is strictly inside margin.
        let inside = diff.abs() < margin;
        return (diff, if inside { 0.0 } else { 1.0 }, if inside { 0.0 } else { 1.0 });
    }
    // H0_lower: diff <= -margin ; reject if (diff + margin)/se is large positive.
    let t_lower = (diff + margin) / se;
    let p_lower = 1.0 - normal_cdf(t_lower);
    // H0_upper: diff >= margin ; reject if (diff - margin)/se is large negative.
    let t_upper = (diff - margin) / se;
    let p_upper = normal_cdf(t_upper);
    (diff, p_lower, p_upper)
}

fn sample_var(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (xs.len() as f64 - 1.0)
}

/// Holm-Bonferroni step-down correction. Given raw p-values, returns, for each
/// input index, (adjusted p-value, rejected at `alpha`). Controls the
/// family-wise error rate across the pre-registered confirmatory contrasts.
pub fn holm(pvalues: &[f64], alpha: f64) -> Vec<(f64, bool)> {
    let m = pvalues.len();
    let mut order: Vec<usize> = (0..m).collect();
    order.sort_by(|&i, &j| pvalues[i].partial_cmp(&pvalues[j]).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = vec![(0.0, false); m];
    let mut running_max = 0.0_f64;
    for (rank, &idx) in order.iter().enumerate() {
        let factor = (m - rank) as f64;
        let adj = (factor * pvalues[idx]).min(1.0).max(running_max);
        running_max = adj;
        out[idx] = (adj, adj <= alpha);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::median;

    #[test]
    fn wilson_matches_examiner_numbers() {
        // 120 of 120 -> lower bound about 0.969.
        let (_, lo, hi) = wilson_interval(120, 120, 1.96);
        assert!((lo - 0.969).abs() < 0.005, "lo={lo}");
        assert!(hi <= 1.0 + 1e-9);
        // 36 of 120 -> about 22.5% to 38.7%.
        let (_, lo, hi) = wilson_interval(36, 120, 1.96);
        assert!((lo - 0.225).abs() < 0.01, "lo={lo}");
        assert!((hi - 0.387).abs() < 0.01, "hi={hi}");
        // 0 of 120 -> upper bound about 3.1%.
        let (_, lo, hi) = wilson_interval(0, 120, 1.96);
        assert!(lo >= 0.0);
        assert!((hi - 0.031).abs() < 0.005, "hi={hi}");
    }

    #[test]
    fn km_median_on_uncensored_data() {
        // Event times 1..=10, all observed -> median survival 5 or 6.
        let data: Vec<Censored> = (1..=10)
            .map(|t| Censored { time: t as f64, event: true })
            .collect();
        let (_curve, med) = kaplan_meier(&data);
        assert!((5.0..=6.0).contains(&med), "median={med}");
    }

    #[test]
    fn logrank_identical_groups_is_insignificant() {
        let a: Vec<Censored> = (1..=30)
            .map(|t| Censored { time: (t % 10 + 1) as f64, event: true })
            .collect();
        let b = a.clone();
        let (chi2, p) = logrank(&a, &b);
        assert!(chi2 < 0.5, "chi2={chi2}");
        assert!(p > 0.3, "p={p}");
    }

    #[test]
    fn logrank_separated_groups_is_significant() {
        let a: Vec<Censored> = (0..30).map(|_| Censored { time: 2.0, event: true }).collect();
        let b: Vec<Censored> = (0..30).map(|_| Censored { time: 50.0, event: true }).collect();
        let (chi2, p) = logrank(&a, &b);
        assert!(chi2 > 10.0, "chi2={chi2}");
        assert!(p < 0.01, "p={p}");
    }

    #[test]
    fn tost_declares_equivalence_for_close_means() {
        let a = vec![1.0, 1.1, 0.9, 1.0, 1.05, 0.95, 1.02, 0.98, 1.0, 1.0];
        let b = vec![1.0, 1.0, 1.0, 1.1, 0.9, 1.0, 1.0, 0.99, 1.01, 1.0];
        let (_diff, pl, pu) = tost_means(&a, &b, 0.5);
        assert!(pl < 0.05 && pu < 0.05, "pl={pl} pu={pu}");
    }

    #[test]
    fn holm_controls_family() {
        let ps = vec![0.01, 0.04, 0.03];
        let adj = holm(&ps, 0.05);
        // smallest 0.01*3=0.03 reject; 0.03*2=0.06 no; 0.04*1=0.04 but monotone -> 0.06.
        assert!(adj[0].1, "0.01 should reject");
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0); // sanity that stats module is linked
    }
}
