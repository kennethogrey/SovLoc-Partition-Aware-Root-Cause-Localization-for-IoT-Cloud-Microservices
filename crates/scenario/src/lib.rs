//! Data-driven experiment scenarios for SovLoc.
//!
//! A *scenario* is a YAML file describing an experiment: how many edge devices
//! there are, which microservices live on each one, how many microservices sit
//! in the cloud backend, and how long each edge device is partitioned from the
//! cloud. It is read once and turned into a [`ScenarioTopology`], a call graph
//! plus a per-service placement and a per-device partition time, which both the
//! simulator (`crates/sim`) and the real mesh (`crates/mesh`) consume. The RCA
//! algorithm in `rca-core` is unchanged; this crate only decides *what topology*
//! it runs over, so the same experiment can be rescaled by editing a file rather
//! than the source.
//!
//! The current hard-coded single-edge system remains the default in both tiers:
//! a scenario is used only when one is explicitly supplied.
//!
//! ```yaml
//! name: five-edge-uneven
//! reps: 30
//! seed: 42
//! loss: 0.0
//! cloud_services: [enrich, store, alerting]
//! edge_devices:
//!   - id: edge-1
//!     services: [gateway, ingest]
//!     partition_time: 10        # seconds this device is cut from the cloud
//!   - id: edge-2
//!     service_count: 5          # auto-named edge-2-svc-0 .. edge-2-svc-4
//!     partition_time: 20
//! ```

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use rca_core::CallGraph;
use serde::Deserialize;

/// Diagnostic ticks per second. The simulator advances in discrete steps where
/// one step is 100 ms, so a partition time given in seconds becomes this many
/// steps. The real mesh uses seconds directly.
pub const STEPS_PER_SEC: f64 = 10.0;

fn default_reps() -> usize {
    30
}
fn default_seed() -> u64 {
    42
}
fn default_fault_at_sec() -> f64 {
    4.0
}
fn default_horizon_sec() -> f64 {
    20.0
}

/// A whole experiment, as read from a YAML file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// A short label used in output rows and generated filenames.
    pub name: String,
    /// Independent repetitions per scenario (the harness requires at least 1; the
    /// exam requires at least 30 for the headline numbers).
    #[serde(default = "default_reps")]
    pub reps: usize,
    /// Base random seed, for reproducibility.
    #[serde(default = "default_seed")]
    pub seed: u64,
    /// Modelled packet loss on the links (0.0 to 1.0), raising measurement noise.
    #[serde(default)]
    pub loss: f64,
    /// When the fault is injected, in seconds from the start of the run.
    #[serde(default = "default_fault_at_sec")]
    pub fault_at_sec: f64,
    /// Total length of the run, in seconds. Must be greater than `fault_at_sec`.
    #[serde(default = "default_horizon_sec")]
    pub horizon_sec: f64,
    /// Optional declared edge-device count, cross-checked against `edge_devices`
    /// so a mismatch between what the file claims and what it lists is caught.
    pub number_of_edge_devices: Option<usize>,
    /// Optional declared total of edge-side microservices, cross-checked against
    /// the sum of the per-device services.
    pub total_microservices: Option<usize>,
    /// Cloud backend microservices by name. Use this or `cloud_service_count`.
    #[serde(default)]
    pub cloud_services: Vec<String>,
    /// Cloud backend microservices by count (auto-named cloud-svc-0 ..). Use this
    /// or `cloud_services`.
    pub cloud_service_count: Option<usize>,
    /// The edge devices, each hosting its own microservices and carrying its own
    /// partition time.
    pub edge_devices: Vec<EdgeDevice>,
    /// Optional explicit call graph. When present it replaces the generated one;
    /// every endpoint must name a microservice declared above.
    #[serde(default)]
    pub edges: Vec<RawEdge>,
}

/// One edge device: an identity, its microservices, and its partition time.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeDevice {
    /// Unique, human-readable device id (also the collector name in the mesh).
    pub id: String,
    /// Microservices on this device by name. Use this or `service_count`.
    #[serde(default)]
    pub services: Vec<String>,
    /// Microservices on this device by count (auto-named `<id>-svc-0` ..). Use
    /// this or `services`.
    pub service_count: Option<usize>,
    /// Seconds this device is cut off from the cloud during the partition. May
    /// differ from every other device. Must be finite and at least 0.
    pub partition_time: f64,
}

/// A call-graph edge in the explicit form, with or without a weight (default
/// weight 1.0). Accepts `[caller, callee]` or `[caller, callee, weight]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawEdge {
    Weighted(String, String, f64),
    Unweighted(String, String),
}

impl RawEdge {
    fn parts(&self) -> (&str, &str, f64) {
        match self {
            RawEdge::Weighted(a, b, w) => (a, b, *w),
            RawEdge::Unweighted(a, b) => (a, b, 1.0),
        }
    }
}

/// Where a microservice runs: the cloud backend, or a specific edge device
/// (by index into [`ScenarioTopology::device_ids`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    Cloud,
    Edge(usize),
}

/// A fully built scenario: the call graph plus everything the harness needs to
/// know about placement and per-device partitioning.
#[derive(Debug, Clone)]
pub struct ScenarioTopology {
    /// The service dependency graph the RCA algorithm walks.
    pub graph: CallGraph,
    /// `placements[i]` says where service `i` runs.
    pub placements: Vec<Placement>,
    /// Device index to device id.
    pub device_ids: Vec<String>,
    /// Device index to partition time in simulator steps.
    pub partition_steps: Vec<usize>,
    /// Device index to partition time in seconds.
    pub partition_sec: Vec<f64>,
    /// `service_device[i]` is the edge device index of service `i`, or `None`
    /// when it is a cloud service.
    pub service_device: Vec<Option<usize>>,
}

impl ScenarioTopology {
    /// Number of microservices in the whole topology (edge plus cloud).
    pub fn n(&self) -> usize {
        self.graph.n
    }

    /// Partition window end for service `i`, in steps, measured from the start of
    /// the run: `fault_at + device partition`. Cloud services return `None`
    /// (never cut). `fault_at` is the step at which the fault (and the partition)
    /// begins.
    pub fn cut_end_step(&self, i: usize, fault_at: usize) -> Option<usize> {
        match self.service_device[i] {
            Some(dev) if self.partition_steps[dev] > 0 => {
                Some(fault_at + self.partition_steps[dev])
            }
            _ => None,
        }
    }
}

impl Scenario {
    /// Read a scenario from a YAML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Scenario> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading scenario file {}", path.display()))?;
        Scenario::from_yaml_str(&text)
            .with_context(|| format!("parsing scenario file {}", path.display()))
    }

    /// Parse a scenario from a YAML string.
    pub fn from_yaml_str(text: &str) -> Result<Scenario> {
        let scenario: Scenario = serde_yaml::from_str(text).context("invalid scenario YAML")?;
        Ok(scenario)
    }

    /// Fault-injection step (seconds to steps).
    pub fn fault_at_steps(&self) -> usize {
        (self.fault_at_sec * STEPS_PER_SEC).round() as usize
    }

    /// Run horizon in steps (seconds to steps).
    pub fn horizon_steps(&self) -> usize {
        (self.horizon_sec * STEPS_PER_SEC).round() as usize
    }

    /// Resolve a device's microservice names, whether listed or counted.
    fn device_services(&self, d: &EdgeDevice) -> Vec<String> {
        if !d.services.is_empty() {
            d.services.clone()
        } else {
            let k = d.service_count.unwrap_or(0);
            (0..k).map(|i| format!("{}-svc-{}", d.id, i)).collect()
        }
    }

    /// Resolve the cloud microservice names, whether listed or counted.
    fn cloud_names(&self) -> Vec<String> {
        if !self.cloud_services.is_empty() {
            self.cloud_services.clone()
        } else {
            let k = self.cloud_service_count.unwrap_or(0);
            (0..k).map(|i| format!("cloud-svc-{}", i)).collect()
        }
    }

    /// Check the scenario is internally consistent. Returns a clear error rather
    /// than silently dropping or renaming anything.
    pub fn validate(&self) -> Result<()> {
        if self.reps == 0 {
            bail!("reps must be at least 1");
        }
        if !self.loss.is_finite() || self.loss < 0.0 {
            bail!(
                "loss must be a finite value at least 0.0 (got {})",
                self.loss
            );
        }
        if !self.fault_at_sec.is_finite() || self.fault_at_sec < 0.0 {
            bail!(
                "fault_at_sec must be finite and at least 0 (got {})",
                self.fault_at_sec
            );
        }
        if !self.horizon_sec.is_finite() || self.horizon_sec <= self.fault_at_sec {
            bail!(
                "horizon_sec ({}) must be finite and greater than fault_at_sec ({})",
                self.horizon_sec,
                self.fault_at_sec
            );
        }

        if self.edge_devices.is_empty() {
            bail!("a scenario needs at least one edge device");
        }
        if let Some(declared) = self.number_of_edge_devices {
            if declared != self.edge_devices.len() {
                bail!(
                    "number_of_edge_devices is {} but {} edge devices are listed",
                    declared,
                    self.edge_devices.len()
                );
            }
        }

        // Cloud must be specified exactly one way and have at least one service:
        // without a cloud backend there is no edge to cloud boundary to partition.
        if !self.cloud_services.is_empty() && self.cloud_service_count.is_some() {
            bail!("set either cloud_services or cloud_service_count, not both");
        }
        let cloud = self.cloud_names();
        if cloud.is_empty() {
            bail!("a scenario needs at least one cloud service (cloud_services or cloud_service_count)");
        }

        // Per-device checks and the running tally for duplicate detection.
        let mut seen: HashMap<String, String> = HashMap::new();
        let note = |name: &str, owner: &str, seen: &mut HashMap<String, String>| -> Result<()> {
            if name.trim().is_empty() {
                bail!("empty microservice name in {}", owner);
            }
            if let Some(prev) = seen.insert(name.to_string(), owner.to_string()) {
                bail!(
                    "duplicate microservice name '{}' (in both {} and {}); \
                     microservice names must be unique across the whole scenario",
                    name,
                    prev,
                    owner
                );
            }
            Ok(())
        };

        let mut device_ids_seen: HashMap<String, ()> = HashMap::new();
        let mut edge_total = 0usize;
        for d in &self.edge_devices {
            if d.id.trim().is_empty() {
                bail!("every edge device needs a non-empty id");
            }
            if device_ids_seen.insert(d.id.clone(), ()).is_some() {
                bail!("duplicate edge-device id '{}'", d.id);
            }
            let listed = !d.services.is_empty();
            let counted = d.service_count.is_some();
            if listed && counted {
                bail!(
                    "edge device '{}' sets both services and service_count; pick one",
                    d.id
                );
            }
            if !listed && !counted {
                bail!(
                    "edge device '{}' has no microservices (set services or service_count)",
                    d.id
                );
            }
            if let Some(c) = d.service_count {
                if c == 0 {
                    bail!("edge device '{}' has service_count 0", d.id);
                }
            }
            if !d.partition_time.is_finite() || d.partition_time < 0.0 {
                bail!(
                    "edge device '{}' has an invalid partition_time {} (must be finite and at least 0)",
                    d.id,
                    d.partition_time
                );
            }
            let names = self.device_services(d);
            edge_total += names.len();
            let owner = format!("edge device '{}'", d.id);
            for name in &names {
                note(name, &owner, &mut seen)?;
            }
        }

        for name in &cloud {
            note(name, "the cloud backend", &mut seen)?;
        }

        if let Some(declared) = self.total_microservices {
            if declared != edge_total {
                bail!(
                    "total_microservices is {} but the edge devices list {} microservices",
                    declared,
                    edge_total
                );
            }
        }

        // Explicit edges, if any, must reference declared microservices.
        for e in &self.edges {
            let (a, b, w) = e.parts();
            if !seen.contains_key(a) {
                bail!("explicit edge references unknown microservice '{}'", a);
            }
            if !seen.contains_key(b) {
                bail!("explicit edge references unknown microservice '{}'", b);
            }
            if !w.is_finite() || w < 0.0 {
                bail!("explicit edge {}->{} has an invalid weight {}", a, b, w);
            }
        }

        Ok(())
    }

    /// Validate, then build the call graph and placement/partition tables.
    pub fn build(&self) -> Result<ScenarioTopology> {
        self.validate()?;

        // Order services as: every edge device's services in device order, then
        // the cloud services. Record placement and device membership per index.
        let mut names: Vec<String> = Vec::new();
        let mut placements: Vec<Placement> = Vec::new();
        let mut service_device: Vec<Option<usize>> = Vec::new();
        let mut device_ids: Vec<String> = Vec::new();
        let mut partition_steps: Vec<usize> = Vec::new();
        let mut partition_sec: Vec<f64> = Vec::new();
        // The service indices of each device's block, so the generated graph can
        // wire the local chains and the boundary crossings.
        let mut device_service_indices: Vec<Vec<usize>> = Vec::new();

        for (dev, d) in self.edge_devices.iter().enumerate() {
            device_ids.push(d.id.clone());
            partition_sec.push(d.partition_time);
            partition_steps.push((d.partition_time * STEPS_PER_SEC).round() as usize);
            let mut idxs = Vec::new();
            for name in self.device_services(d) {
                idxs.push(names.len());
                names.push(name);
                placements.push(Placement::Edge(dev));
                service_device.push(Some(dev));
            }
            device_service_indices.push(idxs);
        }

        let cloud_start = names.len();
        for name in self.cloud_names() {
            names.push(name);
            placements.push(Placement::Cloud);
            service_device.push(None);
        }
        let cloud_idxs: Vec<usize> = (cloud_start..names.len()).collect();

        let mut graph = CallGraph::new(names);

        if self.edges.is_empty() {
            self.generate_edges(&mut graph, &device_service_indices, &cloud_idxs);
        } else {
            for e in &self.edges {
                let (a, b, w) = e.parts();
                // Names were validated to exist, so index_of cannot be None here.
                let ca = graph.index_of(a).expect("validated edge endpoint");
                let cb = graph.index_of(b).expect("validated edge endpoint");
                graph.add_edge(ca, cb, w);
            }
        }

        Ok(ScenarioTopology {
            graph,
            placements,
            device_ids,
            partition_steps,
            partition_sec,
            service_device,
        })
    }

    /// Generate the default IoT call graph: each edge device is a local chain
    /// whose last service crosses into the cloud entry service; the cloud is a
    /// fan-out from its entry service, mirroring the canonical topology.
    fn generate_edges(
        &self,
        graph: &mut CallGraph,
        device_service_indices: &[Vec<usize>],
        cloud_idxs: &[usize],
    ) {
        let cloud_entry = cloud_idxs[0];
        for idxs in device_service_indices {
            for w in idxs.windows(2) {
                graph.add_edge(w[0], w[1], 1.0); // local chain
            }
            let last = *idxs.last().expect("device has at least one service");
            graph.add_edge(last, cloud_entry, 1.0); // cross the edge to cloud boundary
        }
        // Cloud fan-out from the entry: entry to the first leaf at full weight, any
        // further leaves at the lighter alerting-style weight.
        for (pos, &leaf) in cloud_idxs.iter().enumerate().skip(1) {
            let w = if pos == 1 { 1.0 } else { 0.4 };
            graph.add_edge(cloud_entry, leaf, w);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn five_edge_20() -> Scenario {
        let yaml = r#"
name: five-edge-uneven
reps: 30
number_of_edge_devices: 5
total_microservices: 20
cloud_services: [enrich, store, alerting]
edge_devices:
  - { id: edge-1, services: [s-a, s-b],                 partition_time: 10 }
  - { id: edge-2, services: [s-c, s-d, s-e, s-f, s-g],  partition_time: 20 }
  - { id: edge-3, services: [s-h, s-i, s-j],            partition_time: 30 }
  - { id: edge-4, services: [s-k, s-l, s-m, s-n, s-o, s-p, s-q], partition_time: 15 }
  - { id: edge-5, services: [s-r, s-s, s-t],            partition_time: 40 }
"#;
        Scenario::from_yaml_str(yaml).expect("parses")
    }

    #[test]
    fn five_edge_scenario_builds_with_uneven_distribution() {
        let s = five_edge_20();
        let topo = s.build().expect("builds");
        // 5 devices, 20 edge services + 3 cloud = 23 total.
        assert_eq!(topo.device_ids.len(), 5);
        assert_eq!(topo.n(), 23);
        let per_device: Vec<usize> = (0..5)
            .map(|d| {
                topo.service_device
                    .iter()
                    .filter(|sd| **sd == Some(d))
                    .count()
            })
            .collect();
        assert_eq!(per_device, vec![2, 5, 3, 7, 3]);
        // Per-device partition times become steps at 10 steps per second.
        assert_eq!(topo.partition_steps, vec![100, 200, 300, 150, 400]);
    }

    #[test]
    fn ten_edge_thirty_services_is_not_capped() {
        let mut yaml =
            String::from("name: ten-edge\ncloud_services: [c0, c1, c2]\nedge_devices:\n");
        for d in 0..10 {
            // 3 services each gives 30 edge microservices.
            yaml.push_str(&format!(
                "  - {{ id: edge-{}, service_count: 3, partition_time: {} }}\n",
                d,
                (d + 1) * 5
            ));
        }
        let s = Scenario::from_yaml_str(&yaml).expect("parses");
        let topo = s.build().expect("builds");
        assert_eq!(topo.device_ids.len(), 10);
        let edge_count = topo.service_device.iter().filter(|d| d.is_some()).count();
        assert_eq!(edge_count, 30);
        assert_eq!(topo.n(), 33);
    }

    #[test]
    fn auto_named_services_are_unique() {
        let yaml = r#"
name: counted
cloud_service_count: 2
edge_devices:
  - { id: edge-1, service_count: 4, partition_time: 5 }
  - { id: edge-2, service_count: 4, partition_time: 5 }
"#;
        let s = Scenario::from_yaml_str(yaml).expect("parses");
        let topo = s.build().expect("builds");
        assert_eq!(topo.n(), 10);
        assert_eq!(topo.graph.index_of("edge-1-svc-0"), Some(0));
        assert_eq!(topo.graph.index_of("edge-2-svc-3"), Some(7));
    }

    #[test]
    fn cut_end_is_per_device() {
        let s = five_edge_20();
        let topo = s.build().expect("builds");
        let fault_at = 40;
        // A service on edge-1 (partition 100 steps) is cut until step 140.
        let e1 = topo
            .service_device
            .iter()
            .position(|d| *d == Some(0))
            .unwrap();
        assert_eq!(topo.cut_end_step(e1, fault_at), Some(140));
        // A service on edge-5 (partition 400 steps) is cut until step 440.
        let e5 = topo
            .service_device
            .iter()
            .position(|d| *d == Some(4))
            .unwrap();
        assert_eq!(topo.cut_end_step(e5, fault_at), Some(440));
        // Cloud services are never cut.
        let cloud = topo
            .placements
            .iter()
            .position(|p| *p == Placement::Cloud)
            .unwrap();
        assert_eq!(topo.cut_end_step(cloud, fault_at), None);
    }

    #[test]
    fn explicit_edges_round_trip_with_and_without_weight() {
        let yaml = r#"
name: explicit
cloud_services: [enrich]
edge_devices:
  - { id: edge-1, services: [gateway, ingest], partition_time: 10 }
edges:
  - [gateway, ingest, 0.6]
  - [ingest, enrich]
"#;
        let s = Scenario::from_yaml_str(yaml).expect("parses");
        let topo = s.build().expect("builds");
        let g = &topo.graph;
        let gw = g.index_of("gateway").unwrap();
        let ing = g.index_of("ingest").unwrap();
        // The weighted edge keeps its weight; the unweighted edge defaults to 1.0.
        assert!(g.out[gw]
            .iter()
            .any(|(j, w)| *j == ing && (*w - 0.6).abs() < 1e-9));
        let enr = g.index_of("enrich").unwrap();
        assert!(g.out[ing]
            .iter()
            .any(|(j, w)| *j == enr && (*w - 1.0).abs() < 1e-9));
    }

    // validation: every invalid configuration is rejected, not dropped.

    fn err_contains(yaml: &str, needle: &str) {
        let r = Scenario::from_yaml_str(yaml).and_then(|s| s.build());
        let msg = format!("{:#}", r.expect_err("should be rejected"));
        assert!(
            msg.contains(needle),
            "error {:?} should mention {:?}",
            msg,
            needle
        );
    }

    #[test]
    fn rejects_duplicate_service() {
        err_contains(
            r#"
name: dup
cloud_services: [enrich]
edge_devices:
  - { id: edge-1, services: [a, b], partition_time: 10 }
  - { id: edge-2, services: [b, c], partition_time: 10 }
"#,
            "duplicate microservice name 'b'",
        );
    }

    #[test]
    fn rejects_negative_partition_time() {
        err_contains(
            r#"
name: neg
cloud_services: [enrich]
edge_devices:
  - { id: edge-1, services: [a], partition_time: -5 }
"#,
            "invalid partition_time",
        );
    }

    #[test]
    fn rejects_device_with_no_services() {
        err_contains(
            r#"
name: empty
cloud_services: [enrich]
edge_devices:
  - { id: edge-1, partition_time: 10 }
"#,
            "no microservices",
        );
    }

    #[test]
    fn rejects_declared_device_count_mismatch() {
        err_contains(
            r#"
name: mismatch
number_of_edge_devices: 3
cloud_services: [enrich]
edge_devices:
  - { id: edge-1, services: [a], partition_time: 10 }
  - { id: edge-2, services: [b], partition_time: 10 }
"#,
            "number_of_edge_devices is 3 but 2",
        );
    }

    #[test]
    fn rejects_declared_total_microservices_mismatch() {
        err_contains(
            r#"
name: mismatch
total_microservices: 20
cloud_services: [enrich]
edge_devices:
  - { id: edge-1, services: [a, b], partition_time: 10 }
"#,
            "total_microservices is 20 but the edge devices list 2",
        );
    }

    #[test]
    fn rejects_unknown_explicit_edge_endpoint() {
        err_contains(
            r#"
name: badedge
cloud_services: [enrich]
edge_devices:
  - { id: edge-1, services: [a], partition_time: 10 }
edges:
  - [a, nonexistent]
"#,
            "unknown microservice 'nonexistent'",
        );
    }

    #[test]
    fn rejects_horizon_not_after_fault() {
        err_contains(
            r#"
name: badtime
fault_at_sec: 10
horizon_sec: 8
cloud_services: [enrich]
edge_devices:
  - { id: edge-1, services: [a], partition_time: 10 }
"#,
            "must be finite and greater than fault_at_sec",
        );
    }

    #[test]
    fn rejects_missing_cloud() {
        err_contains(
            r#"
name: nocloud
edge_devices:
  - { id: edge-1, services: [a], partition_time: 10 }
"#,
            "at least one cloud service",
        );
    }
}
