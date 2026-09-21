//! One-sided CUSUM change-point detector.
//!
//! This answers "*when* did something start going wrong?", the anomaly-onset
//! time. Separating detection-onset from localization lets us report
//! **detection latency** and **localization latency** as two different numbers
//! (a recommendation in your plan: most reviewed papers conflate them).
//!
//! CUSUM (cumulative sum) keeps a running sum of how far each new sample sits
//! above the in-control mean; when that sum crosses a threshold, a sustained
//! shift has occurred. It is a classic non-AI statistical tool, no training.

/// Detect the first index of a sustained upward shift in `series`.
///
/// The first `baseline_len` samples estimate the in-control mean and standard
/// deviation. `k` is the slack (in std units) that a sample must exceed before
/// it accumulates (typical 0.5); `h` is the decision threshold in std units
/// (typical 4 to 5). Returns `None` if no change is detected.
pub fn cusum_first_change(series: &[f64], baseline_len: usize, k: f64, h: f64) -> Option<usize> {
    if baseline_len == 0 || series.len() <= baseline_len {
        return None;
    }
    let base = &series[..baseline_len];
    let mean = base.iter().sum::<f64>() / base.len() as f64;
    let var = base.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / base.len() as f64;
    let std = var.sqrt() + 1e-9;

    let mut s = 0.0_f64;
    for (t, &x) in series.iter().enumerate().skip(baseline_len) {
        s = (s + (x - mean) - k * std).max(0.0);
        if s > h * std {
            return Some(t);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_step_change() {
        let mut series = vec![1.0; 30];
        for v in series.iter_mut().skip(20) {
            *v = 20.0; // big step at t=20
        }
        let idx = cusum_first_change(&series, 15, 0.5, 4.0).unwrap();
        assert!((20..=22).contains(&idx), "detected at {idx}");
    }

    #[test]
    fn no_change_returns_none() {
        let series = vec![5.0; 40];
        assert!(cusum_first_change(&series, 15, 0.5, 4.0).is_none());
    }
}
