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
//! The same algorithm runs in every placement in the experiment. What differs
//! between placements is not a hand-set visibility flag but which telemetry each
//! collector has actually **received** by a given step, which the simulator's
//! transport model decides (queue, retry, replay on heal, loss, bandwidth). The
//! `observed[i]` mask passed to [`localize`] therefore reflects data a collector
//! holds, not an assumption, and a service whose samples have not arrived simply
//! has a stale, low anomaly score.

pub mod changepoint;
pub mod graph;
pub mod ppr;
pub mod stats;
pub mod survival;

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
    /// Robust-deviation threshold the BARO baseline uses to decide which services
    /// have actually changed; only services at or above it are ranked by that
    /// method, falling back to all anomalous services when none cross it. Matches
    /// the detector's default `h` so BARO sees the same change set.
    pub detect_h: f64,
}

impl Default for RcaConfig {
    fn default() -> Self {
        RcaConfig {
            alpha: 0.85,
            iters: 100,
            self_weight: 1.0,
            eps: 1e-6,
            detect_h: 4.0,
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

/// Random-walk-with-restart localization over the call graph, used as a non-AI
/// graph baseline (the MicroRCA / MonitorRank / RCD / CIRCA family in the survey).
///
/// Unlike [`localize`], which follows **forward** edges only and settles on the
/// deepest anomalous service, this walker moves over the **undirected** graph:
/// from a service it can step to any caller or callee, biased toward the more
/// anomalous neighbour, and it restarts into the anomaly distribution. Walking in
/// both directions is the classic symptom-to-cause random walk, so it ranks a
/// shared or amplifying neighbour differently from the forward PPR walk, which is
/// exactly the comparison the ablation needs.
pub fn random_walk_localize(
    graph: &CallGraph,
    anomaly: &[f64],
    visible: &[bool],
    cfg: &RcaConfig,
) -> Vec<RankedCause> {
    let n = graph.n;
    let mut a = vec![0.0_f64; n];
    for i in 0..n {
        if visible[i] {
            a[i] = anomaly[i].max(0.0);
        }
    }
    let total: f64 = a.iter().sum();
    if total <= 0.0 {
        return Vec::new();
    }
    let personalization: Vec<f64> = a.iter().map(|x| x / total).collect();

    let mut trans: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    for i in 0..n {
        if !visible[i] {
            continue;
        }
        let mut outs: Vec<(usize, f64)> = Vec::new();
        // Both directions: callees (out) and callers (incoming).
        for &(j, w) in graph.out[i].iter().chain(graph.incoming[i].iter()) {
            if visible[j] {
                let wt = w * (a[j] + cfg.eps);
                if wt > 0.0 {
                    outs.push((j, wt));
                }
            }
        }
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
/// `None` if the culprit is absent (e.g., its telemetry has not been received).
pub fn rank_of(ranked: &[RankedCause], culprit: usize) -> Option<usize> {
    ranked
        .iter()
        .position(|r| r.service == culprit)
        .map(|p| p + 1)
}

/// The rankers compared in the ablation (hypothesis H4). `Argmax` and `Ppr` are
/// our ablation pair (does the graph help?); `Baro` and `RandomWalk` are the two
/// non-AI literature baselines the review asks us to beat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ranker {
    /// Plain argmax of the robust anomaly score over the observed services. No
    /// graph is used, so a caller whose anomaly is amplified by retries can
    /// outscore the true culprit.
    Argmax,
    /// BARO (Pham et al.): rank by robust-deviation magnitude, restricted to the
    /// services whose deviation crossed the detector threshold (`cfg.detect_h`),
    /// which is the change set BARO's detector would hand its RobustScorer. Its
    /// localization step is argmax of the robust score, so once every anomalous
    /// service is well above threshold it coincides with `Argmax`; the two can
    /// differ early, while some services are still sub-threshold. No graph.
    Baro,
    /// A random-walk-with-restart over the undirected call graph
    /// ([`random_walk_localize`]), standing for the MicroRCA / MonitorRank / RCD /
    /// CIRCA graph-causal family. Walks toward callers and callees alike, so it is
    /// a genuine graph competitor that differs from the forward-only PPR walk.
    RandomWalk,
    /// Personalized PageRank over the call graph ([`localize`]), which follows
    /// the dependency structure toward the deepest anomalous service.
    Ppr,
}

/// Rank candidate root causes with the chosen ranker. `observed[i]` is true when
/// the collector currently holds telemetry for service `i`; unobserved services
/// are never ranked. For [`Ranker::Argmax`] the ranking is just the observed
/// services sorted by anomaly; for [`Ranker::Ppr`] it is [`localize`].
pub fn rank_causes(
    graph: &CallGraph,
    anomaly: &[f64],
    observed: &[bool],
    cfg: &RcaConfig,
    ranker: Ranker,
) -> Vec<RankedCause> {
    match ranker {
        Ranker::Ppr => localize(graph, anomaly, observed, cfg),
        Ranker::RandomWalk => random_walk_localize(graph, anomaly, observed, cfg),
        Ranker::Argmax => argmax_rank(graph.n, anomaly, observed, 0.0),
        // BARO ranks only the services whose robust deviation crossed the detector
        // threshold; if none have (very early, or a weak fault), fall back to all
        // anomalous services so a ranking still exists.
        Ranker::Baro => {
            let r = argmax_rank(graph.n, anomaly, observed, cfg.detect_h);
            if r.is_empty() {
                argmax_rank(graph.n, anomaly, observed, 0.0)
            } else {
                r
            }
        }
    }
}

/// Rank observed services with anomaly strictly above `floor` by descending
/// anomaly. Shared by the `Argmax` (floor 0) and `Baro` (floor = threshold) paths.
fn argmax_rank(n: usize, anomaly: &[f64], observed: &[bool], floor: f64) -> Vec<RankedCause> {
    let mut ranked: Vec<RankedCause> = (0..n)
        .filter(|&i| observed[i] && anomaly[i].max(0.0) > floor)
        .map(|i| RankedCause {
            service: i,
            score: anomaly[i],
        })
        .collect();
    ranked.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked
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
    fn ppr_beats_statistical_baselines_on_retry_amplification() {
        // Culprit is the deepest service (2), but its immediate caller (1) is MORE
        // anomalous because retries amplified it. The graph-blind methods pick the
        // caller; the forward PPR walk still settles on the deepest anomalous node.
        let g = chain();
        let cfg = RcaConfig::default();
        let vis = vec![true, true, true];
        let anomaly = vec![0.0, 10.0, 5.0]; // caller 1 > culprit 2 > gateway 0
        let culprit = 2;

        let argmax = rank_causes(&g, &anomaly, &vis, &cfg, Ranker::Argmax);
        let baro = rank_causes(&g, &anomaly, &vis, &cfg, Ranker::Baro);
        let ppr = rank_causes(&g, &anomaly, &vis, &cfg, Ranker::Ppr);

        assert_eq!(argmax[0].service, 1, "argmax picks the amplified caller");
        assert_eq!(baro[0].service, 1, "BARO also picks the amplified caller");
        assert_eq!(
            ppr[0].service, culprit,
            "PPR recovers the true deepest culprit, got {ppr:?}"
        );
    }

    #[test]
    fn random_walk_is_a_distinct_graph_method() {
        // The undirected random walk and the forward PPR walk must not be the same
        // function: on an amplified-caller profile they disagree on the top-1.
        let g = chain();
        let cfg = RcaConfig::default();
        let vis = vec![true, true, true];
        let anomaly = vec![0.0, 10.0, 5.0];
        let rw = rank_causes(&g, &anomaly, &vis, &cfg, Ranker::RandomWalk);
        let ppr = rank_causes(&g, &anomaly, &vis, &cfg, Ranker::Ppr);
        assert!(!rw.is_empty() && !ppr.is_empty());
        assert_ne!(
            rw[0].service, ppr[0].service,
            "random walk should rank differently from PPR here (rw={rw:?}, ppr={ppr:?})"
        );
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
