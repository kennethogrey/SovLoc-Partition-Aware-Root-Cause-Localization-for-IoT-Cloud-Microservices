//! # rca-core
//!
//! Non-AI root-cause localization for microservice systems. Two ideas from your
//! survey, combined:
//!
//! 1. **Robust change-point scoring** ([`stats`], [`changepoint`]), decide
//!    *when* a fault began and *how anomalous* each service is, using only
//!    medians/IQR and CUSUM. No training, edge-friendly (survey class B).
//! 2. **Personalized PageRank over the call graph** ([`ppr`], [`localize`]),
//!    decide *which* service is the root cause, by walking the dependency graph
//!    biased toward the anomalies (survey class D).
//!
//! The same algorithm powers both the **centralised** and **decentralised**
//! analyzers in the experiment; the *only* difference between them is which
//! services' telemetry is **visible** (`visible[i]`). Under a network
//! partition, a centralised cloud collector loses visibility of the edge side,
//! while a decentralised edge agent keeps it, which is exactly why the
//! decentralised design preserves *operational sovereignty*.

pub mod changepoint;
pub mod graph;
pub mod ppr;
pub mod stats;

pub use graph::CallGraph;

/// Tunables for the localization step.
#[derive(Clone, Debug)]
pub struct RcaConfig {
    /// PageRank damping (follow-edge probability).
    pub alpha: f64,
    /// Power-iteration steps.
    pub iters: usize,
    /// How strongly a service "keeps" rank proportional to its own anomaly
    /// (relative to passing rank downstream). Makes a terminal culprit sticky.
    pub self_weight: f64,
    /// Numerical epsilon.
    pub eps: f64,
}

impl Default for RcaConfig {
    fn default() -> Self {
        RcaConfig {
            alpha: 0.85,
            iters: 100,
            self_weight: 1.0,
            eps: 1e-6,
        }
    }
}

/// One ranked candidate root cause.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RankedCause {
    pub service: usize,
    pub score: f64,
}

/// Rank services by how likely each is the root cause, given per-service
/// `anomaly` scores and a `visible` mask (telemetry the analyzer can actually
/// see). Returns candidates in descending score order; the first is the top-1
/// culprit. Returns an empty vector if no visible service is anomalous (nothing
/// to localize yet).
///
/// **Mechanism.** A latency/resource fault at service *F* inflates *F*'s own
/// metric and, because callers wait on *F*, the metrics of *F*'s ancestors,
/// but not *F*'s callees. So we let the walker follow **forward** edges
/// (caller → callee), weighting each step toward the more anomalous callee, and
/// give every node a self-loop weighted by its own anomaly. Rank therefore
/// slides down the anomalous chain and settles on the deepest anomalous node,
/// the culprit, where it can go no further.
pub fn localize(
    graph: &CallGraph,
    anomaly: &[f64],
    visible: &[bool],
    cfg: &RcaConfig,
) -> Vec<RankedCause> {
    let n = graph.n;
    assert_eq!(anomaly.len(), n);
    assert_eq!(visible.len(), n);

    // Effective anomaly: a service the analyzer cannot see contributes nothing.
    let mut a = vec![0.0_f64; n];
    for i in 0..n {
        if visible[i] {
            a[i] = anomaly[i].max(0.0);
        }
    }
    let total: f64 = a.iter().sum();
    if total <= 0.0 {
        return Vec::new(); // no anomaly is visible → cannot localize
    }
    let personalization: Vec<f64> = a.iter().map(|x| x / total).collect();

    // Build anomaly-biased forward transitions among visible services.
    let mut trans: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    for i in 0..n {
        if !visible[i] {
            continue;
        }
        let mut outs: Vec<(usize, f64)> = Vec::new();
        for &(j, w) in &graph.out[i] {
            if !visible[j] {
                continue;
            }
            let wt = w * (a[j] + cfg.eps); // prefer moving toward an anomalous callee
            if wt > 0.0 {
                outs.push((j, wt));
            }
        }
        // Self-loop weighted by own anomaly: a terminal culprit retains mass.
        outs.push((i, cfg.self_weight * (a[i] + cfg.eps)));

        let s: f64 = outs.iter().map(|x| x.1).sum();
        if s > 0.0 {
            for o in outs.iter_mut() {
                o.1 /= s;
            }
        }
        trans[i] = outs;
    }

    let rank = ppr::personalized_pagerank(n, &trans, &personalization, cfg.alpha, cfg.iters);

    let mut ranked: Vec<RankedCause> = (0..n)
        .filter(|&i| visible[i])
        .map(|i| RankedCause {
            service: i,
            score: rank[i],
        })
        .collect();
    ranked.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked
}

/// Convenience: the 1-based rank of `culprit` in a ranking (1 = top-1). Returns
/// `None` if the culprit is absent (e.g., invisible under partition).
pub fn rank_of(ranked: &[RankedCause], culprit: usize) -> Option<usize> {
    ranked
        .iter()
        .position(|r| r.service == culprit)
        .map(|p| p + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a 3-service chain: 0 (gateway) -> 1 (ingest) -> 2 (store).
    fn chain() -> CallGraph {
        let mut g = CallGraph::new(vec!["gateway".into(), "ingest".into(), "store".into()]);
        g.add_edge(0, 1, 1.0);
        g.add_edge(1, 2, 1.0);
        g
    }

    // Anomaly profile for a fault at `culprit`: strongest at the culprit,
    // attenuating upward to its ancestors (they wait on it), zero downstream.
    fn anomaly_for(culprit: usize) -> Vec<f64> {
        match culprit {
            0 => vec![10.0, 0.0, 0.0], // gateway broken; downstream fine
            1 => vec![4.0, 10.0, 0.0], // ingest broken; gateway waits; store fine
            2 => vec![2.0, 4.0, 10.0], // store broken; both ancestors wait
            _ => unreachable!(),
        }
    }

    #[test]
    fn localizes_fault_at_each_position() {
        let g = chain();
        let cfg = RcaConfig::default();
        let all_visible = vec![true, true, true];
        for culprit in 0..3 {
            let a = anomaly_for(culprit);
            let ranked = localize(&g, &a, &all_visible, &cfg);
            assert_eq!(
                ranked[0].service, culprit,
                "top-1 should be {culprit}, got {:?}",
                ranked
            );
        }
    }

    #[test]
    fn partition_hides_edge_side_culprit_from_central_but_not_local() {
        // Fault at `store` (index 2), which sits on the edge side of a partition.
        let g = chain();
        let cfg = RcaConfig::default();
        let a = anomaly_for(2);

        // Centralised cloud collector: edge side (ingest, store) is invisible.
        let central_visible = vec![true, false, false];
        let central = localize(&g, &a, &central_visible, &cfg);
        assert_ne!(
            central.first().map(|r| r.service),
            Some(2),
            "centralised must NOT localize the hidden edge-side culprit"
        );

        // Decentralised edge agent: the edge component is fully visible locally.
        let local_visible = vec![false, true, true];
        let local = localize(&g, &a, &local_visible, &cfg);
        assert_eq!(
            local[0].service, 2,
            "decentralised must still localize the culprit under partition"
        );
    }
}
