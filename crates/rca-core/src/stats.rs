//! Small robust-statistics helpers used by the anomaly scorer.
//!
//! "Robust" means resistant to outliers: we use the median and the
//! inter-quartile range (IQR) instead of the mean and standard deviation,
//! because a single spike should not move our baseline. This mirrors the
//! RobustScorer idea from the BARO paper reviewed in your survey (a non-AI,
//! training-free statistical method, survey decision class B).

/// The `q`-quantile (0.0..=1.0) of `data`, with linear interpolation.
/// `quantile(x, 0.5)` is the median.
pub fn quantile(data: &[f64], q: f64) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut v = data.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pos = q.clamp(0.0, 1.0) * (v.len() as f64 - 1.0);
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        v[lo]
    } else {
        let frac = pos - lo as f64;
        v[lo] * (1.0 - frac) + v[hi] * frac
    }
}

/// Median = the 50th percentile.
pub fn median(data: &[f64]) -> f64 {
    quantile(data, 0.5)
}

/// Inter-quartile range = 75th percentile − 25th percentile.
pub fn iqr(data: &[f64]) -> f64 {
    quantile(data, 0.75) - quantile(data, 0.25)
}

/// BARO-style robust deviation score of the current value `x` from a
/// pre-fault `baseline` window. Higher = more anomalous.
///
/// score = |x − median(baseline)| / (IQR(baseline)/1.349 + eps)
///
/// The 1.349 converts an IQR into an estimate of the standard deviation for a
/// normal distribution, so the score reads roughly like a robust z-score. The
/// epsilon avoids division by zero when a metric is perfectly flat.
pub fn robust_score(baseline: &[f64], x: f64) -> f64 {
    if baseline.is_empty() {
        return 0.0;
    }
    let med = median(baseline);
    let sigma = iqr(baseline) / 1.349 + 1e-9;
    (x - med).abs() / sigma
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_known_set() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
    }

    #[test]
    fn flat_baseline_gives_zero_then_large_score() {
        let base = vec![10.0; 20];
        // identical to baseline → ~0
        assert!(robust_score(&base, 10.0) < 1e-3);
        // far from baseline → large
        assert!(robust_score(&base, 100.0) > 10.0);
    }
}
