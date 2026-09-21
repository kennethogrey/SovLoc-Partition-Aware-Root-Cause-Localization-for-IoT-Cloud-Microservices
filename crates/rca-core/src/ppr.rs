//! Personalized PageRank (PPR) via power iteration.
//!
//! PageRank models a random walker on a graph. "Personalized" means that when
//! the walker restarts, it jumps back to a chosen distribution (here: the
//! anomalous services) rather than uniformly. Running this over the service
//! call graph, biased toward anomalies, concentrates rank on the true culprit
//!, a non-AI, graph-centrality method (your survey decision class D; the same
//! family as MicroNet and the decentralised-PageRank RCA of Fernando et al.).

/// Personalized PageRank.
///
/// * `trans[i]`, outgoing transition probabilities from node `i` as
///   `(target, prob)` pairs; each node's probs should sum to ≈ 1. A node with
///   an empty list is "dangling" and its mass teleports to `personalization`.
/// * `personalization[i]`, the restart distribution (should sum to 1).
/// * `alpha`, damping: follow an edge with prob `alpha`, restart with prob
///   `1 − alpha`. 0.85 is the classic value.
/// * `iters`, number of power-iteration steps (100 is ample for tiny graphs).
///
/// Returns a score per node (sums to ≈ 1); higher = more likely root cause.
pub fn personalized_pagerank(
    n: usize,
    trans: &[Vec<(usize, f64)>],
    personalization: &[f64],
    alpha: f64,
    iters: usize,
) -> Vec<f64> {
    assert_eq!(trans.len(), n);
    assert_eq!(personalization.len(), n);

    let mut rank = personalization.to_vec();
    for _ in 0..iters {
        let mut next = vec![0.0_f64; n];

        // Total rank sitting on dangling (no-outgoing) nodes this step.
        let dangling_mass: f64 = (0..n)
            .filter(|&i| trans[i].is_empty())
            .map(|i| rank[i])
            .sum();

        for i in 0..n {
            // Restart component + redistribution of dangling mass.
            next[i] += (1.0 - alpha) * personalization[i];
            next[i] += alpha * dangling_mass * personalization[i];
        }

        // Edge-following component.
        for i in 0..n {
            if trans[i].is_empty() {
                continue;
            }
            let ri = rank[i];
            if ri == 0.0 {
                continue;
            }
            for &(j, p) in &trans[i] {
                next[j] += alpha * ri * p;
            }
        }

        rank = next;
    }
    rank
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mass_is_conserved() {
        // 0 -> 1 -> 2 chain.
        let trans = vec![vec![(1usize, 1.0)], vec![(2usize, 1.0)], vec![]];
        let pers = vec![1.0, 0.0, 0.0];
        let r = personalized_pagerank(3, &trans, &pers, 0.85, 200);
        let sum: f64 = r.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "sum={sum}");
    }
}
