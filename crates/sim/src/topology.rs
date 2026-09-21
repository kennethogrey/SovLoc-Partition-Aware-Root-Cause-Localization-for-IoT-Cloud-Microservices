//! The simulated IoT microservice topology and its edge/cloud split.
//!
//! Shape (base, 7 services): an IoT ingestion pipeline.
//!
//! ```text
//!   [EDGE, near the sensors]              |  [CLOUD backend]
//!   gateway → ingest → validate ───────────→ enrich → store
//!                        │                 |            ↑
//!                        └→ device-registry|  alerting ←┘
//! ```
//!
//! The **partition** cuts the single edge↔cloud link. Edge-side services are
//! the ones that, in rural-Uganda operating conditions, must keep working when
//! the backhaul to the cloud drops (your primary sense: *operational
//! sovereignty*).

use rca_core::CallGraph;

/// Which side of the edge↔cloud boundary a service sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Edge,
    Cloud,
}

/// A built topology: the call graph plus each service's side.
pub struct Topology {
    pub graph: CallGraph,
    pub sides: Vec<Side>,
}

/// Build a topology with `n` services (n is clamped to at least 7). Sizes above
/// 7 add extra edge-side sensor-gateway replicas feeding `ingest`, modelling a
/// larger rural cluster.
pub fn build(n: usize) -> Topology {
    let n = n.max(7);
    let mut names: Vec<String> = vec![
        "gateway".into(),         // 0 edge
        "ingest".into(),          // 1 edge
        "validate".into(),        // 2 edge
        "device-registry".into(), // 3 edge
        "enrich".into(),          // 4 cloud
        "store".into(),           // 5 cloud
        "alerting".into(),        // 6 cloud
    ];
    for r in 0..(n - 7) {
        names.push(format!("gateway-replica-{r}")); // extra edge gateways
    }

    let mut sides = vec![
        Side::Edge,  // gateway
        Side::Edge,  // ingest
        Side::Edge,  // validate
        Side::Edge,  // device-registry
        Side::Cloud, // enrich
        Side::Cloud, // store
        Side::Cloud, // alerting
    ];
    for _ in 0..(n - 7) {
        sides.push(Side::Edge);
    }

    let mut g = CallGraph::new(names);
    // Base pipeline edges (caller → callee, weight = relative call frequency).
    g.add_edge(0, 1, 1.0); // gateway → ingest
    g.add_edge(1, 2, 1.0); // ingest → validate
    g.add_edge(2, 3, 0.6); // validate → device-registry
    g.add_edge(2, 4, 1.0); // validate → enrich   (crosses the boundary)
    g.add_edge(4, 5, 1.0); // enrich → store
    g.add_edge(4, 6, 0.4); // enrich → alerting
                           // Extra edge gateways also feed ingest.
    for idx in 7..n {
        g.add_edge(idx, 1, 0.5);
    }

    Topology { graph: g, sides }
}

/// The one edge index that carries the request path across the boundary:
/// `validate (edge) → enrich (cloud)`. Used only for documentation/plots.
#[allow(dead_code)]
pub const BOUNDARY_EDGE: (usize, usize) = (2, 4);
