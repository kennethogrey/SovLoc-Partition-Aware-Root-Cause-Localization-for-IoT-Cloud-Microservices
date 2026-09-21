//! The service dependency (call) graph.
//!
//! Nodes are microservices; a directed edge `caller → callee` with a weight
//! (how often the call happens) records that one service invokes another. This
//! is the structure that root-cause ranking walks over, it is exactly the
//! "Layer 2 (API & Service)" evidence in your nine-layer model, reconstructed
//! from distributed traces/spans.

/// A directed, weighted call graph over `n` services.
#[derive(Clone, Debug)]
pub struct CallGraph {
    pub n: usize,
    pub names: Vec<String>,
    /// `out[i]` = callees of service `i`, each with a call weight.
    pub out: Vec<Vec<(usize, f64)>>,
    /// `incoming[j]` = callers of service `j`, each with a call weight.
    pub incoming: Vec<Vec<(usize, f64)>>,
}

impl CallGraph {
    /// Create an edge-less graph with the given service names.
    pub fn new(names: Vec<String>) -> Self {
        let n = names.len();
        CallGraph {
            n,
            names,
            out: vec![Vec::new(); n],
            incoming: vec![Vec::new(); n],
        }
    }

    /// Record that `caller` invokes `callee` with the given call weight.
    pub fn add_edge(&mut self, caller: usize, callee: usize, weight: f64) {
        self.out[caller].push((callee, weight));
        self.incoming[callee].push((caller, weight));
    }

    /// Human-readable name of a service index.
    pub fn name(&self, i: usize) -> &str {
        &self.names[i]
    }

    /// Index of a service by name (linear scan; graphs here are tiny).
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|s| s == name)
    }
}
