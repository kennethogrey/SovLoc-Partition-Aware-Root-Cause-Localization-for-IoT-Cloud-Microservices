//! Shared topology + metric model for the real mesh. This mirrors the
//! deterministic simulator (`crates/sim`) so that the production tier tests the
//! *same* system on real infrastructure, the only differences are that the
//! network, the partition (via `tc/netem`), and the two collectors are real.

use rca_core::CallGraph;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Edge,
    Cloud,
}

// Real-time versions of the simulator's constants (seconds instead of steps).
pub const BASE_LAT: f64 = 20.0;
pub const FAULT_MAG: f64 = 60.0;
pub const ATTEN: f64 = 0.5;
pub const HOP_DELAY_S: f64 = 0.4; // per-hop propagation delay (seconds)
pub const TAU_S: f64 = 1.5; // fault ramp time constant (seconds)

/// The seven canonical services and their sides (matches `sim::topology`).
pub fn services() -> Vec<(&'static str, Side)> {
    vec![
        ("gateway", Side::Edge),
        ("ingest", Side::Edge),
        ("validate", Side::Edge),
        ("device-registry", Side::Edge),
        ("enrich", Side::Cloud),
        ("store", Side::Cloud),
        ("alerting", Side::Cloud),
    ]
}

/// Build the call graph (same edges as the simulator).
pub fn build_graph() -> CallGraph {
    let names: Vec<String> = services().iter().map(|(n, _)| n.to_string()).collect();
    let mut g = CallGraph::new(names);
    g.add_edge(0, 1, 1.0); // gateway -> ingest
    g.add_edge(1, 2, 1.0); // ingest -> validate
    g.add_edge(2, 3, 0.6); // validate -> device-registry
    g.add_edge(2, 4, 1.0); // validate -> enrich (crosses boundary)
    g.add_edge(4, 5, 1.0); // enrich -> store
    g.add_edge(4, 6, 0.4); // enrich -> alerting
    g
}

/// Distance from `culprit` up the caller chain; `Some(0)` at the culprit,
/// `Some(d)` at an ancestor d hops up, `None` if unaffected.
pub fn ancestor_distances(g: &CallGraph, culprit: usize) -> Vec<Option<usize>> {
    let n = g.n;
    let mut dist = vec![None; n];
    dist[culprit] = Some(0);
    let mut q = std::collections::VecDeque::new();
    q.push_back(culprit);
    while let Some(u) = q.pop_front() {
        let du = dist[u].unwrap();
        for &(caller, _w) in &g.incoming[u] {
            if dist[caller].is_none() {
                dist[caller] = Some(du + 1);
                q.push_back(caller);
            }
        }
    }
    dist
}

/// The metric value a service should report `secs_since_fault` seconds after the
/// fault was injected at `culprit` (negative before injection). `noise` is a
/// fresh random sample supplied by the caller.
pub fn service_value(idx: usize, dist: &[Option<usize>], secs_since_fault: f64, noise: f64) -> f64 {
    let base = BASE_LAT + (idx % 3) as f64;
    let mut v = base + noise;
    if secs_since_fault >= 0.0 {
        if let Some(d) = dist[idx] {
            let onset = d as f64 * HOP_DELAY_S;
            if secs_since_fault >= onset {
                let ramp = 1.0 - (-(secs_since_fault - onset) / TAU_S).exp();
                v += FAULT_MAG * ATTEN.powi(d as i32) * ramp;
            }
        }
    }
    v.max(0.0)
}
