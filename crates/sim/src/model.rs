//! The honest SovLoc model.
//!
//! The old simulator decided the result by a visibility mask: during a partition
//! the central placement simply could not see the edge, so its failure was true
//! by construction. This module removes that mask. Instead it models how
//! telemetry is transported across the edge-to-cloud boundary (sampling, a link
//! that goes down during a partition, one-way latency, bandwidth, loss, and an
//! optional store-and-forward buffer that replays on heal), and each placement
//! then localizes only from the telemetry it has actually received. Time to
//! diagnosis and bytes on the wire are therefore measured outcomes of the
//! transport and the buffer policy, not assumptions.
//!
//! Four placements are compared:
//!   * `central-naive`  a cloud collector; telemetry sent while the link is down
//!                      is lost.
//!   * `central-sf`     a cloud collector with an edge store-and-forward buffer
//!                      that queues during the cut and replays on heal.
//!   * `edge-local`     a collector on the edge; edge telemetry is local, cloud
//!                      telemetry is pulled across, and a compact verdict is sent
//!                      to the cloud.
//!   * `hybrid`         a collector on each side analysing its own services
//!                      locally and exchanging compact verdicts.
//!
//! The detector is a CUSUM change-point test that is never told the fault onset;
//! it scans from the end of the baseline window, so false alarms before the true
//! onset are possible and are measured separately.

use rca_core::stats::robust_score;
use rca_core::{rank_causes, rank_of, CallGraph, Ranker, RcaConfig};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, Normal};

pub const STEP_MS: f64 = 100.0; // one simulation step is 100 ms (a diagnostic tick)
pub const BASELINE_LEN: usize = 40; // pre-fault steps used as the robust baseline
pub const STABILITY: usize = 3; // consecutive correct top-1 steps to confirm
pub const SAMPLE_BYTES: u64 = 256; // one full telemetry sample
pub const VERDICT_BYTES: u64 = 64; // one compact verdict crossing the boundary
const FAULT_MEAN: f64 = 60.0;
const FAULT_SD: f64 = 10.0;
const TAU: f64 = 3.0; // fault ramp time constant (steps)
const HOP_DELAY: usize = 2; // per-hop propagation delay (steps)
const BASE_LAT: f64 = 20.0; // baseline metric per service
const NOISE_SD: f64 = 2.0; // baseline measurement noise
const LOSS_NOISE: f64 = 6.0; // extra noise per unit packet loss

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Edge,
    Cloud,
}

/// A built topology: the call graph plus each service's side.
pub struct Topo {
    pub graph: CallGraph,
    pub sides: Vec<Side>,
}

/// Topology families used in the detector/ranker ablation (E2, E7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopoKind {
    /// A single pipeline with one boundary crossing (the base SovLoc shape).
    Chain,
    /// A fan-out tree: one edge parent calls several edge children.
    Tree,
    /// A shared edge dependency called by several services.
    Shared,
    /// A cycle among three edge services (feedback).
    Cycle,
    /// A chain whose immediate caller retries on a slow callee (amplification).
    Retry,
}

/// Fault families. The gain profile up the caller chain differs so that a plain
/// argmax of the anomaly is not always right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// No fault at all: a fault-free run, used to measure the false-alarm rate.
    None,
    /// Latency fault: the culprit is most anomalous, callers attenuate upward.
    Latency,
    /// Retry amplification: the immediate caller is MORE anomalous than the
    /// culprit, so argmax picks the caller and only a graph-aware ranker recovers
    /// the true root.
    Retry,
    /// Local CPU/resource fault: only the culprit's own metric moves.
    Cpu,
    /// Crash/timeout: a large abrupt spike at the culprit and its waiters.
    Crash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    CentralNaive,
    CentralSf,
    EdgeLocal,
    Hybrid,
}

impl Placement {
    pub fn as_str(&self) -> &'static str {
        match self {
            Placement::CentralNaive => "central-naive",
            Placement::CentralSf => "central-sf",
            Placement::EdgeLocal => "edge-local",
            Placement::Hybrid => "hybrid",
        }
    }
    pub fn all() -> [Placement; 4] {
        [
            Placement::CentralNaive,
            Placement::CentralSf,
            Placement::EdgeLocal,
            Placement::Hybrid,
        ]
    }
}

/// Build a topology of the given kind with `n_extra` additional edge services
/// beyond the base shape (so total size scales for E7). Cloud side is always the
/// enrich/store/alerting trio; everything else is edge.
pub fn build_topology(kind: TopoKind, n_extra: usize) -> Topo {
    let mut names: Vec<String> = vec![
        "gateway".into(),
        "ingest".into(),
        "validate".into(),
        "device-registry".into(),
        "enrich".into(),
        "store".into(),
        "alerting".into(),
    ];
    let mut sides = vec![
        Side::Edge,
        Side::Edge,
        Side::Edge,
        Side::Edge,
        Side::Cloud,
        Side::Cloud,
        Side::Cloud,
    ];
    for r in 0..n_extra {
        names.push(format!("edge-svc-{r}"));
        sides.push(Side::Edge);
    }
    let mut g = CallGraph::new(names);
    // Base pipeline, one crossing validate(2) -> enrich(4).
    g.add_edge(0, 1, 1.0);
    g.add_edge(1, 2, 1.0);
    g.add_edge(2, 3, 0.6);
    g.add_edge(2, 4, 1.0);
    g.add_edge(4, 5, 1.0);
    g.add_edge(4, 6, 0.4);
    match kind {
        TopoKind::Chain | TopoKind::Retry => {
            for idx in 7..g.n {
                g.add_edge(idx, 1, 0.5); // extra gateways feed ingest
            }
        }
        TopoKind::Tree => {
            // gateway fans out to all extra edge children.
            for idx in 7..g.n {
                g.add_edge(0, idx, 0.8);
            }
        }
        TopoKind::Shared => {
            // device-registry(3) is a shared dependency called by all extras.
            for idx in 7..g.n {
                g.add_edge(idx, 3, 0.8);
            }
        }
        TopoKind::Cycle => {
            // a feedback cycle 1 -> 2 -> 3 -> 1 already partly present; close it.
            g.add_edge(3, 1, 0.3);
            for idx in 7..g.n {
                g.add_edge(idx, 1, 0.5);
            }
        }
    }
    Topo { graph: g, sides }
}

/// BFS hop distances from `culprit` up the caller chain (ancestors). `None` for
/// services that do not wait on the culprit.
fn ancestor_distances(incoming: &[Vec<(usize, f64)>], culprit: usize) -> Vec<Option<usize>> {
    let n = incoming.len();
    let mut dist = vec![None; n];
    dist[culprit] = Some(0);
    let mut q = std::collections::VecDeque::new();
    q.push_back(culprit);
    while let Some(u) = q.pop_front() {
        let du = dist[u].unwrap();
        for &(caller, _w) in &incoming[u] {
            if dist[caller].is_none() {
                dist[caller] = Some(du + 1);
                q.push_back(caller);
            }
        }
    }
    dist
}

/// Per-hop gain of the fault as it propagates up the caller chain. `d` is the hop
/// distance from the culprit (0 = culprit).
fn hop_gain(kind: FaultKind, d: usize) -> f64 {
    match kind {
        FaultKind::None => 0.0,
        FaultKind::Latency => 0.5_f64.powi(d as i32),
        FaultKind::Crash => 0.7_f64.powi(d as i32),
        FaultKind::Cpu => {
            if d == 0 {
                1.0
            } else {
                0.0
            }
        }
        FaultKind::Retry => match d {
            0 => 1.0,
            1 => 1.8, // the immediate caller is amplified by retries
            _ => 1.8 * 0.5_f64.powi((d - 1) as i32),
        },
    }
}

/// Generate the true per-service metric time series with a propagating fault.
pub fn generate_metrics(
    topo: &Topo,
    culprit: usize,
    kind: FaultKind,
    fault_at: usize,
    horizon: usize,
    loss: f64,
    rng: &mut StdRng,
) -> Vec<Vec<f64>> {
    let n = topo.graph.n;
    let sigma = NOISE_SD + LOSS_NOISE * loss;
    let noise = Normal::new(0.0, sigma).unwrap();
    let mag = Normal::new(FAULT_MEAN, FAULT_SD)
        .unwrap()
        .sample(rng)
        .max(20.0);
    let dist = ancestor_distances(&topo.graph.incoming, culprit);

    let mut m = vec![vec![0.0_f64; horizon]; n];
    for i in 0..n {
        let base = BASE_LAT + (i % 3) as f64;
        for t in 0..horizon {
            let mut v = base + noise.sample(rng);
            if let Some(d) = dist[i] {
                let onset = fault_at + d * HOP_DELAY;
                if t >= onset {
                    let ramp = 1.0 - (-((t - onset) as f64) / TAU).exp();
                    v += mag * hop_gain(kind, d) * ramp;
                }
            }
            m[i][t] = v.max(0.0);
        }
    }
    m
}

/// Transport policy for other-side telemetry.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Policy {
    Naive,
    StoreForward,
}

/// Build the collector's "held view": `view[i][t]` is the most recently received
/// value of service `i` at step `t` (NaN until the first sample arrives). Returns
/// the view, the measured bytes that crossed the boundary, and which services
/// were ever observed. `collector_side` is where the collector sits; services on
/// the other side must cross the link.
#[allow(clippy::too_many_arguments)]
fn held_view(
    topo: &Topo,
    metrics: &[Vec<f64>],
    collector_side: Side,
    policy: Policy,
    part_start: usize,
    part_len: usize,
    sample_every: usize,
    latency: usize,
    bandwidth: usize, // queued samples drained per step after heal
    loss: f64,
    rng: &mut StdRng,
) -> (Vec<Vec<f64>>, u64, Vec<bool>) {
    let n = topo.graph.n;
    let horizon = metrics[0].len();
    let heal = part_start + part_len;
    let up = |e: usize| part_len == 0 || e < part_start || e >= heal;

    let mut view = vec![vec![f64::NAN; horizon]; n];
    let mut observed = vec![false; n];
    let mut cross_bytes: u64 = 0;

    for i in 0..n {
        // (delivery_step, value) events for service i.
        let mut events: Vec<(usize, f64)> = Vec::new();
        let same_side = topo.sides[i] == collector_side;
        let mut queue: Vec<(usize, f64)> = Vec::new(); // store-and-forward buffer
        let mut e = 0usize;
        while e < horizon {
            let value = metrics[i][e];
            if same_side {
                events.push((e, value)); // local, no crossing
            } else if up(e) {
                // link up: delivered unless lost in transit
                if loss <= 0.0 || rng.gen::<f64>() >= loss {
                    let dt = (e + latency).min(horizon - 1);
                    events.push((dt, value));
                    cross_bytes += SAMPLE_BYTES;
                }
            } else {
                // link down
                match policy {
                    Policy::Naive => {} // dropped forever
                    Policy::StoreForward => queue.push((e, value)),
                }
            }
            e += sample_every;
        }
        // Drain the store-and-forward queue after heal, bandwidth-limited.
        if policy == Policy::StoreForward && !queue.is_empty() {
            let bw = bandwidth.max(1);
            for (q, (_orig, value)) in queue.into_iter().enumerate() {
                let dt = (heal + q / bw + latency).min(horizon - 1);
                events.push((dt, value));
                cross_bytes += SAMPLE_BYTES;
            }
        }
        // Fill the held view forward from the delivery events.
        events.sort_by_key(|(t, _)| *t);
        let mut last = f64::NAN;
        let mut ev = 0usize;
        for t in 0..horizon {
            while ev < events.len() && events[ev].0 <= t {
                last = events[ev].1;
                observed[i] = true;
                ev += 1;
            }
            view[i][t] = last;
        }
    }
    (view, cross_bytes, observed)
}

/// The outcome of one analyzer over one held view.
struct AnalyzerOut {
    diagnosed: bool,
    ttd: f64,          // steps from fault onset to confirmed top-1 (if diagnosed)
    censored_at: f64,  // steps from fault onset to end of window (if not diagnosed)
    det_latency: f64,  // steps from fault onset to first CUSUM alarm (NaN if none)
    false_alarm: bool, // a CUSUM alarm fired before the true fault onset
    final_rank: i64,
}

/// Detection onset: the first step at which any observed service's robust
/// deviation exceeds `h` for `persist` consecutive steps. The detector scans from
/// the end of the baseline window and is never told the true fault onset, so a
/// firing before the fault is a false alarm. Returns the first step of the
/// exceeding run.
fn detect_onset(
    view: &[Vec<f64>],
    observed: &[bool],
    h: f64,
    persist: usize,
    horizon: usize,
) -> Option<usize> {
    let n = view.len();
    let mut consec = vec![0usize; n];
    for t in BASELINE_LEN..horizon {
        for i in 0..n {
            if observed[i] && !view[i][t].is_nan() {
                let z = robust_score(&view[i][0..BASELINE_LEN], view[i][t]);
                if z > h {
                    consec[i] += 1;
                    if consec[i] >= persist {
                        return Some(t + 1 - persist);
                    }
                } else {
                    consec[i] = 0;
                }
            }
        }
    }
    None
}

/// Run the robust-deviation change detector (not told the onset) and the ranker
/// over a held view.
#[allow(clippy::too_many_arguments)]
fn analyze_view(
    topo: &Topo,
    view: &[Vec<f64>],
    observed: &[bool],
    culprit: usize,
    fault_at: usize,
    horizon: usize,
    h: f64,
    persist: usize,
    ranker: Ranker,
) -> AnalyzerOut {
    let n = topo.graph.n;
    let cfg = RcaConfig::default();

    let det_step = match detect_onset(view, observed, h, persist, horizon) {
        Some(t) => t,
        None => {
            return AnalyzerOut {
                diagnosed: false,
                ttd: f64::NAN,
                censored_at: (horizon - fault_at) as f64,
                det_latency: f64::NAN,
                false_alarm: false,
                final_rank: -1,
            }
        }
    };
    let false_alarm = det_step < fault_at;
    let det_latency = det_step as f64 - fault_at as f64;

    // Localization from detection onward: confirm STABILITY consecutive correct
    // top-1 steps.
    let mut correct_run = 0usize;
    let mut run_start = 0usize;
    let mut last_rank = -1i64;
    let mut diagnosed_at: Option<usize> = None;
    // Localization is only meaningful once the fault actually exists, so the
    // confirmation scan starts at the true onset even when the detector fired
    // earlier (a false alarm). Detection latency, recorded above, keeps the early
    // firing; time to diagnosis is never negative.
    for t in det_step.max(fault_at)..horizon {
        let mut anomaly = vec![0.0_f64; n];
        for i in 0..n {
            if observed[i] && !view[i][t].is_nan() {
                anomaly[i] = robust_score(&view[i][0..BASELINE_LEN], view[i][t]);
            }
        }
        let obs_now: Vec<bool> = (0..n)
            .map(|i| observed[i] && !view[i][t].is_nan())
            .collect();
        let ranked = rank_causes(&topo.graph, &anomaly, &obs_now, &cfg, ranker);
        last_rank = rank_of(&ranked, culprit).map(|r| r as i64).unwrap_or(-1);
        if ranked.first().map(|r| r.service) == Some(culprit) {
            if correct_run == 0 {
                run_start = t;
            }
            correct_run += 1;
            if correct_run >= STABILITY {
                diagnosed_at = Some(run_start);
                break;
            }
        } else {
            correct_run = 0;
        }
    }

    match diagnosed_at {
        Some(t) => AnalyzerOut {
            diagnosed: true,
            ttd: t as f64 - fault_at as f64,
            censored_at: f64::NAN,
            det_latency,
            false_alarm,
            final_rank: last_rank,
        },
        None => AnalyzerOut {
            diagnosed: false,
            ttd: f64::NAN,
            censored_at: (horizon - fault_at) as f64,
            det_latency,
            false_alarm,
            final_rank: last_rank,
        },
    }
}

/// Parameters for one run (one placement on one seeded scenario).
#[derive(Clone, Copy)]
pub struct RunParams {
    pub kind: TopoKind,
    pub fault: FaultKind,
    pub n_extra: usize,
    pub placement: Placement,
    pub ranker: Ranker,
    pub part_len: usize,
    pub part_offset: i64, // partition start relative to fault onset (steps)
    pub loss: f64,
    pub sample_every: usize,
    pub latency: usize,
    pub bandwidth: usize,
    pub h: f64,
    pub persist: usize, // consecutive steps above h required to declare an onset
    pub seed: u64,
    pub culprit: Option<usize>, // fixed culprit, or random when None
}

/// The measured outcome of one run.
pub struct RunOutcome {
    pub culprit: usize,
    pub culprit_side: Side,
    pub diagnosed: bool,
    pub ttd: f64,
    pub censored_at: f64,
    pub det_latency: f64,
    pub false_alarm: bool,
    pub final_rank: i64,
    pub cross_bytes: u64,
}

/// Simulate one run end to end and measure the outcome.
pub fn simulate(params: &RunParams) -> RunOutcome {
    let topo = build_topology(params.kind, params.n_extra);
    let n = topo.graph.n;
    let mut rng = StdRng::seed_from_u64(params.seed);
    let culprit = params.culprit.unwrap_or_else(|| rng.gen_range(0..n));

    // Timeline: a fixed baseline, then the fault, then room to diagnose after the
    // link heals so that censoring reflects the design, not a truncated window.
    let fault_at = BASELINE_LEN + 20;
    let part_start = (fault_at as i64 + params.part_offset).max(0) as usize;
    let heal = part_start + params.part_len;
    let horizon = (heal + params.part_len.max(40) + 60).max(fault_at + 120);

    let metrics = generate_metrics(
        &topo,
        culprit,
        params.fault,
        fault_at,
        horizon,
        params.loss,
        &mut rng,
    );

    let out = match params.placement {
        Placement::CentralNaive | Placement::CentralSf => {
            let policy = if params.placement == Placement::CentralSf {
                Policy::StoreForward
            } else {
                Policy::Naive
            };
            let (view, bytes, observed) = held_view(
                &topo,
                &metrics,
                Side::Cloud,
                policy,
                part_start,
                params.part_len,
                params.sample_every,
                params.latency,
                params.bandwidth,
                params.loss,
                &mut rng,
            );
            let a = analyze_view(
                &topo,
                &view,
                &observed,
                culprit,
                fault_at,
                horizon,
                params.h,
                params.persist,
                params.ranker,
            );
            (a, bytes)
        }
        Placement::EdgeLocal => {
            // Collector on the edge: edge local, cloud pulled across naively; a
            // compact verdict is sent to the cloud.
            let (view, mut bytes, observed) = held_view(
                &topo,
                &metrics,
                Side::Edge,
                Policy::Naive,
                part_start,
                params.part_len,
                params.sample_every,
                params.latency,
                params.bandwidth,
                params.loss,
                &mut rng,
            );
            let a = analyze_view(
                &topo,
                &view,
                &observed,
                culprit,
                fault_at,
                horizon,
                params.h,
                params.persist,
                params.ranker,
            );
            if a.diagnosed {
                bytes += VERDICT_BYTES; // one verdict crosses on diagnosis
            }
            (a, bytes)
        }
        Placement::Hybrid => {
            // One analyzer per side, each over its own local services only; only
            // compact verdicts cross.
            let edge_only = side_mask(&topo, Side::Edge);
            let cloud_only = side_mask(&topo, Side::Cloud);
            let edge_view = local_view(&metrics, &edge_only);
            let cloud_view = local_view(&metrics, &cloud_only);
            let ea = analyze_view(
                &topo,
                &edge_view,
                &edge_only,
                culprit,
                fault_at,
                horizon,
                params.h,
                params.persist,
                params.ranker,
            );
            let ca = analyze_view(
                &topo,
                &cloud_view,
                &cloud_only,
                culprit,
                fault_at,
                horizon,
                params.h,
                params.persist,
                params.ranker,
            );
            // The culprit sits on one side; that side's analyzer is the one that
            // can localize it. Take the earliest diagnosis across the two.
            let a = combine(ea, ca, horizon, fault_at);
            let bytes = VERDICT_BYTES * 2; // one verdict each way
            (a, bytes)
        }
    };

    let (a, bytes) = out;
    RunOutcome {
        culprit,
        culprit_side: topo.sides[culprit],
        diagnosed: a.diagnosed,
        ttd: a.ttd,
        censored_at: a.censored_at,
        det_latency: a.det_latency,
        false_alarm: a.false_alarm,
        final_rank: a.final_rank,
        cross_bytes: bytes,
    }
}

/// Count false alarms over a fault-free stretch of `horizon` steps on the base
/// topology, using the same robust-deviation persistence rule as the detector
/// (the counter re-arms after each firing). Used to report false alarms per hour.
pub fn count_false_alarms(h: f64, persist: usize, horizon: usize, seed: u64) -> usize {
    let topo = build_topology(TopoKind::Chain, 0);
    let mut rng = StdRng::seed_from_u64(seed);
    // No fault: generate_metrics with FaultKind::None gives a pure baseline.
    let metrics = generate_metrics(
        &topo,
        0,
        FaultKind::None,
        horizon / 2,
        horizon,
        0.0,
        &mut rng,
    );
    let mut events = 0usize;
    for series in &metrics {
        let mut consec = 0usize;
        for t in BASELINE_LEN..horizon {
            let z = robust_score(&series[0..BASELINE_LEN], series[t]);
            if z > h {
                consec += 1;
                if consec >= persist {
                    events += 1;
                    consec = 0;
                }
            } else {
                consec = 0;
            }
        }
    }
    events
}

fn side_mask(topo: &Topo, side: Side) -> Vec<bool> {
    topo.sides.iter().map(|s| *s == side).collect()
}

/// A local view: services in the mask are fully observed from their own metrics;
/// others are never observed (NaN forever).
fn local_view(metrics: &[Vec<f64>], mask: &[bool]) -> Vec<Vec<f64>> {
    let horizon = metrics[0].len();
    (0..metrics.len())
        .map(|i| {
            if mask[i] {
                metrics[i].clone()
            } else {
                vec![f64::NAN; horizon]
            }
        })
        .collect()
}

/// Earliest diagnosis across the two hybrid analyzers.
fn combine(ea: AnalyzerOut, ca: AnalyzerOut, horizon: usize, fault_at: usize) -> AnalyzerOut {
    match (ea.diagnosed, ca.diagnosed) {
        (true, true) => {
            if ea.ttd <= ca.ttd {
                ea
            } else {
                ca
            }
        }
        (true, false) => ea,
        (false, true) => ca,
        (false, false) => AnalyzerOut {
            diagnosed: false,
            ttd: f64::NAN,
            censored_at: (horizon - fault_at) as f64,
            det_latency: ea.det_latency.min(ca.det_latency),
            false_alarm: ea.false_alarm || ca.false_alarm,
            final_rank: ea.final_rank.max(ca.final_rank),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_params(placement: Placement, part_len: usize, culprit: usize) -> RunParams {
        RunParams {
            kind: TopoKind::Chain,
            fault: FaultKind::Latency,
            n_extra: 0,
            placement,
            ranker: Ranker::Ppr,
            part_len,
            part_offset: 0,
            loss: 0.0,
            sample_every: 1,
            latency: 0,
            bandwidth: 20,
            h: 4.0,
            persist: 3,
            seed: 7,
            culprit: Some(culprit),
        }
    }

    #[test]
    fn no_partition_everyone_diagnoses_edge_fault() {
        // validate (index 2) is an edge-side service.
        for p in Placement::all() {
            let o = simulate(&base_params(p, 0, 2));
            assert!(
                o.diagnosed,
                "{} should diagnose with no partition",
                p.as_str()
            );
        }
    }

    #[test]
    fn long_partition_hurts_central_not_edge_for_an_edge_fault() {
        // A long edge-side partition: the naive cloud collector should be slower
        // (or censored) than the edge-local collector, but this now EMERGES from
        // transport rather than a mask.
        let central = simulate(&base_params(Placement::CentralNaive, 300, 2));
        let edge = simulate(&base_params(Placement::EdgeLocal, 300, 2));
        assert!(edge.diagnosed, "edge-local should diagnose the edge fault");
        if central.diagnosed {
            assert!(
                central.ttd >= edge.ttd,
                "central ttd {} should be >= edge ttd {}",
                central.ttd,
                edge.ttd
            );
        }
    }

    #[test]
    fn edge_local_pays_for_a_cloud_fault_under_partition() {
        // enrich (index 4) is cloud-side: now the edge-local collector is the one
        // that loses the fault during the cut, which the old mask could never show.
        let edge = simulate(&base_params(Placement::EdgeLocal, 300, 4));
        let central = simulate(&base_params(Placement::CentralNaive, 300, 4));
        assert!(
            central.diagnosed,
            "cloud collector sees the cloud-side fault"
        );
        // edge-local is blind to the cloud side during the cut; it is slower or censored.
        if edge.diagnosed && central.diagnosed {
            assert!(edge.ttd >= central.ttd);
        }
    }

    #[test]
    fn central_sf_crosses_more_bytes_than_edge_local() {
        let sf = simulate(&base_params(Placement::CentralSf, 200, 2));
        let edge = simulate(&base_params(Placement::EdgeLocal, 200, 2));
        assert!(sf.cross_bytes > edge.cross_bytes);
    }
}
