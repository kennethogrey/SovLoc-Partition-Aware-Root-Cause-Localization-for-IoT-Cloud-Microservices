//! SovLoc deterministic simulation harness.
//!
//! Compares two RCA topologies over the *same* generated telemetry:
//!   * **central**, a single cloud collector; loses sight of the edge side
//!                     during a partition.
//!   * **decentral**, per-side edge agents; the component holding the culprit
//!                     keeps localizing during a partition.
//!
//! Everything is seeded, so the exact same command reproduces the exact same
//! numbers (exam §4 reproducibility). Nothing here is invented: we set explicit,
//! documented modelling assumptions and *measure* the outcome. The production
//! tier (`crates/mesh`) tests whether the direction of the result survives on a
//! real network. The production tier is the mesh crate and the docker
//! compose demo (see the README).
//!
//! Run:  `cargo run -p sim --release`  (writes results/raw.csv + results/summary.csv)

mod topology;

use std::collections::VecDeque;
use std::fs;

use anyhow::Result;
use clap::Parser;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, Normal};
use serde::Serialize;

use rca_core::stats::robust_score;
use rca_core::{localize, rank_of, RcaConfig};
use topology::{build, Side};

// ---- Fixed experiment constants (documented modelling assumptions) ----------
const T: usize = 200; // total timesteps; 1 step ≈ 100 ms (a diagnostic tick)
const T_FAULT: usize = 40; // when the fault is injected
const BASELINE_LEN: usize = 40; // pre-fault window used as the robust baseline
const DETECT_THR: f64 = 4.0; // robust-score threshold (~4σ) for anomaly onset
const STABILITY: usize = 3; // consecutive correct top-1 steps to confirm
const FAULT_MEAN: f64 = 60.0; // mean fault magnitude added to the culprit's metric
const FAULT_SD: f64 = 10.0;
const ATTEN: f64 = 0.5; // per-hop attenuation of the anomaly up to ancestors
const HOP_DELAY: usize = 2; // per-hop propagation delay (timesteps)
const TAU: f64 = 3.0; // fault ramp-up time constant
const BASE_LAT: f64 = 20.0; // baseline metric value per service
const NOISE_SD: f64 = 2.0; // baseline measurement noise
const LOSS_NOISE: f64 = 6.0; // extra noise per unit packet-loss (DDIL)
const MSG_FULL: u64 = 256; // bytes: a full telemetry sample to the collector
const MSG_GOSSIP: u64 = 64; // bytes: a compact gossiped anomaly score

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    Central,
    Decentral,
}
impl Method {
    fn as_str(&self) -> &'static str {
        match self {
            Method::Central => "central",
            Method::Decentral => "decentral",
        }
    }
}

#[derive(Parser, Debug)]
#[command(about = "SovLoc simulation: centralised vs decentralised RCA under partition")]
struct Cli {
    /// Independent repetitions per configuration (exam requires ≥30).
    #[arg(long, default_value_t = 30)]
    reps: usize,
    /// Base random seed (reproducibility).
    #[arg(long, default_value_t = 42)]
    seed: u64,
    /// Output directory for raw.csv and summary.csv.
    #[arg(long, default_value = "results")]
    out_dir: String,
    /// Quick smoke run (fewer configs/reps) for a fast sanity check.
    #[arg(long)]
    quick: bool,
}

/// One row of raw results (one method × one repetition).
#[derive(Serialize)]
struct RepResult {
    method: String,
    n: usize,
    partition_dur: usize,
    loss: f64,
    seed: u64,
    culprit: String,
    culprit_side: String,
    detected: bool,
    detection_latency: i64,    // timesteps; -1 = never detected
    localized: bool,           // correct top-1 confirmed within horizon
    localization_latency: i64, // timesteps from fault; -1 = censored/failed
    localized_during_partition: bool,
    culprit_rank: i64,          // final rank of true culprit; -1 = absent
    total_telemetry_bytes: u64, // modelled bandwidth proxy
    cross_boundary_bytes: u64,  // modelled data crossing the edge↔cloud link
}

/// BFS distances from `culprit` up the caller chain. `dist[i] = Some(d)` if `i`
/// is the culprit (d=0) or an ancestor (caller, transitively) at hop distance d;
/// `None` if `i` is unaffected (a callee of the culprit, or unrelated).
fn ancestor_distances(incoming: &[Vec<(usize, f64)>], culprit: usize) -> Vec<Option<usize>> {
    let n = incoming.len();
    let mut dist = vec![None; n];
    dist[culprit] = Some(0);
    let mut q = VecDeque::new();
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

/// Generate per-service metric time series with an injected, propagating fault.
fn generate_metrics(
    topo: &topology::Topology,
    culprit: usize,
    loss: f64,
    rng: &mut StdRng,
) -> Vec<Vec<f64>> {
    let n = topo.graph.n;
    let sigma = NOISE_SD + LOSS_NOISE * loss;
    let noise = Normal::new(0.0, sigma).unwrap();
    let fault_mag = Normal::new(FAULT_MEAN, FAULT_SD)
        .unwrap()
        .sample(rng)
        .max(20.0);
    let dist = ancestor_distances(&topo.graph.incoming, culprit);

    let mut m = vec![vec![0.0_f64; T]; n];
    for i in 0..n {
        let base = BASE_LAT + (i % 3) as f64; // slight per-service offset
        for t in 0..T {
            let mut v = base + noise.sample(rng);
            if let Some(d) = dist[i] {
                let onset = T_FAULT + d * HOP_DELAY;
                if t >= onset {
                    let ramp = 1.0 - (-((t - onset) as f64) / TAU).exp();
                    v += fault_mag * ATTEN.powi(d as i32) * ramp;
                }
            }
            m[i][t] = v.max(0.0);
        }
    }
    m
}

/// Is service `i` visible to `method`'s analyzer at time `t`?
fn visible_at(
    method: Method,
    sides: &[Side],
    culprit: usize,
    i: usize,
    t: usize,
    dur: usize,
) -> bool {
    let partitioned = dur > 0 && t >= T_FAULT && t < T_FAULT + dur;
    match method {
        // Cloud collector: cloud side always visible; edge side only when the link is up.
        Method::Central => sides[i] == Side::Cloud || !partitioned,
        // Edge agent: within a partition it sees its own component (the culprit's side).
        Method::Decentral => !partitioned || sides[i] == sides[culprit],
    }
}

struct Outcome {
    detected: bool,
    det_lat: i64,
    localized: bool,
    loc_lat: i64,
    loc_during: bool,
    culprit_rank: i64,
}

/// Run one analyzer over the pre-generated telemetry.
fn analyze(
    method: Method,
    topo: &topology::Topology,
    metrics: &[Vec<f64>],
    culprit: usize,
    dur: usize,
) -> Outcome {
    let n = topo.graph.n;
    let cfg = RcaConfig::default();

    let mut detected = false;
    let mut det_lat = -1i64;
    let mut localized = false;
    let mut loc_lat = -1i64;
    let mut loc_during = false;
    let mut correct_run = 0usize;
    let mut run_start = 0usize;
    let mut last_rank = -1i64;

    for t in T_FAULT..T {
        // Per-service anomaly scores against the pre-fault baseline window.
        let mut anomaly = vec![0.0_f64; n];
        for i in 0..n {
            anomaly[i] = robust_score(&metrics[i][0..BASELINE_LEN], metrics[i][t]);
        }
        // Visibility mask for this method at this time.
        let vis: Vec<bool> = (0..n)
            .map(|i| visible_at(method, &topo.sides, culprit, i, t, dur))
            .collect();

        if !detected {
            let maxv = (0..n)
                .filter(|&i| vis[i])
                .map(|i| anomaly[i])
                .fold(0.0_f64, f64::max);
            if maxv > DETECT_THR {
                detected = true;
                det_lat = (t - T_FAULT) as i64;
            }
        }

        if detected {
            let ranked = localize(&topo.graph, &anomaly, &vis, &cfg);
            let top1 = ranked.first().map(|r| r.service);
            last_rank = rank_of(&ranked, culprit).map(|r| r as i64).unwrap_or(-1);

            if top1 == Some(culprit) {
                if correct_run == 0 {
                    run_start = t;
                }
                correct_run += 1;
            } else {
                correct_run = 0;
            }
            if !localized && correct_run >= STABILITY {
                localized = true;
                loc_lat = (run_start - T_FAULT) as i64;
                loc_during = dur > 0 && run_start < T_FAULT + dur;
            }
        }
    }

    Outcome {
        detected,
        det_lat,
        localized,
        loc_lat,
        loc_during,
        culprit_rank: last_rank,
    }
}

/// Modelled bandwidth proxies for a method over a topology (independent of the
/// partition; a structural sovereignty cost).
fn bandwidth(method: Method, topo: &topology::Topology) -> (u64, u64) {
    let n = topo.graph.n as u64;
    let edges: u64 = topo.graph.out.iter().map(|o| o.len() as u64).sum();
    let edge_services = topo.sides.iter().filter(|s| **s == Side::Edge).count() as u64;
    match method {
        Method::Central => {
            // Every service streams full samples to the cloud each step; all
            // edge-side telemetry crosses the boundary.
            let total = n * T as u64 * MSG_FULL;
            let cross = edge_services * T as u64 * MSG_FULL;
            (total, cross)
        }
        Method::Decentral => {
            // Compact gossip along each edge (both directions), staying on-side;
            // only two small verdicts cross the boundary.
            let total = 2 * edges * T as u64 * MSG_GOSSIP;
            let cross = 2 * MSG_FULL;
            (total, cross)
        }
    }
}

struct Config {
    n: usize,
    dur: usize,
    loss: f64,
}

/// Run both methods on one shared, seeded scenario.
fn run_pair(cfg: &Config, seed: u64) -> (RepResult, RepResult) {
    let topo = build(cfg.n);
    let mut rng = StdRng::seed_from_u64(seed);
    let culprit = rng.gen_range(0..topo.graph.n);
    let metrics = generate_metrics(&topo, culprit, cfg.loss, &mut rng);

    let culprit_name = topo.graph.name(culprit).to_string();
    let culprit_side = match topo.sides[culprit] {
        Side::Edge => "edge",
        Side::Cloud => "cloud",
    };

    let mut rows = Vec::with_capacity(2);
    for method in [Method::Central, Method::Decentral] {
        let o = analyze(method, &topo, &metrics, culprit, cfg.dur);
        let (total_b, cross_b) = bandwidth(method, &topo);
        rows.push(RepResult {
            method: method.as_str().to_string(),
            n: cfg.n,
            partition_dur: cfg.dur,
            loss: cfg.loss,
            seed,
            culprit: culprit_name.clone(),
            culprit_side: culprit_side.to_string(),
            detected: o.detected,
            detection_latency: o.det_lat,
            localized: o.localized,
            localization_latency: o.loc_lat,
            localized_during_partition: o.loc_during,
            culprit_rank: o.culprit_rank,
            total_telemetry_bytes: total_b,
            cross_boundary_bytes: cross_b,
        });
    }
    let decentral = rows.pop().unwrap();
    let central = rows.pop().unwrap();
    (central, decentral)
}

// ---- Summary statistics -----------------------------------------------------
#[derive(Serialize)]
struct SummaryRow {
    method: String,
    partition_dur: usize,
    n_reps: usize,
    success_rate: f64,         // fraction localized within horizon
    mean_loc_latency: f64,     // over localized reps only (timesteps)
    ci95_loc_latency: f64,     // half-width of 95% CI
    edge_culprit_success: f64, // success rate restricted to edge-side culprits
    mean_cross_boundary_kb: f64,
}

fn mean_ci(values: &[f64]) -> (f64, f64) {
    let k = values.len();
    if k == 0 {
        return (f64::NAN, f64::NAN);
    }
    let mean = values.iter().sum::<f64>() / k as f64;
    if k < 2 {
        return (mean, 0.0);
    }
    let var = values.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (k as f64 - 1.0);
    let ci = 1.96 * var.sqrt() / (k as f64).sqrt();
    (mean, ci)
}

fn summarize(rows: &[RepResult], durs: &[usize]) -> Vec<SummaryRow> {
    let mut out = Vec::new();
    for method in ["central", "decentral"] {
        for &dur in durs {
            let group: Vec<&RepResult> = rows
                .iter()
                .filter(|r| r.method == method && r.partition_dur == dur)
                .collect();
            if group.is_empty() {
                continue;
            }
            let n_reps = group.len();
            let success_rate = group.iter().filter(|r| r.localized).count() as f64 / n_reps as f64;
            let lats: Vec<f64> = group
                .iter()
                .filter(|r| r.localized)
                .map(|r| r.localization_latency as f64)
                .collect();
            let (mean_lat, ci) = mean_ci(&lats);
            let edge: Vec<&&RepResult> =
                group.iter().filter(|r| r.culprit_side == "edge").collect();
            let edge_success = if edge.is_empty() {
                f64::NAN
            } else {
                edge.iter().filter(|r| r.localized).count() as f64 / edge.len() as f64
            };
            let mean_cross = group
                .iter()
                .map(|r| r.cross_boundary_bytes as f64)
                .sum::<f64>()
                / n_reps as f64
                / 1024.0;
            out.push(SummaryRow {
                method: method.to_string(),
                partition_dur: dur,
                n_reps,
                success_rate,
                mean_loc_latency: mean_lat,
                ci95_loc_latency: ci,
                edge_culprit_success: edge_success,
                mean_cross_boundary_kb: mean_cross,
            });
        }
    }
    out
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let (ns, durs, losses, reps): (Vec<usize>, Vec<usize>, Vec<f64>, usize) = if cli.quick {
        (vec![7], vec![0, 80, 160], vec![0.0], 5.min(cli.reps).max(3))
    } else {
        (
            vec![7, 12],
            vec![0, 20, 40, 80, 160],
            vec![0.0, 0.1],
            cli.reps,
        )
    };

    fs::create_dir_all(&cli.out_dir)?;
    let raw_path = format!("{}/raw.csv", cli.out_dir);
    let summary_path = format!("{}/summary.csv", cli.out_dir);

    let mut all_rows: Vec<RepResult> = Vec::new();
    let mut config_index: u64 = 0;
    for &n in &ns {
        for &dur in &durs {
            for &loss in &losses {
                let cfg = Config { n, dur, loss };
                for rep in 0..reps {
                    let seed = cli.seed + config_index * 10_000 + rep as u64;
                    let (c, d) = run_pair(&cfg, seed);
                    all_rows.push(c);
                    all_rows.push(d);
                }
                config_index += 1;
            }
        }
    }

    // Write raw rows.
    let mut w = csv::Writer::from_path(&raw_path)?;
    for r in &all_rows {
        w.serialize(r)?;
    }
    w.flush()?;

    // Summarize and write.
    let summary = summarize(&all_rows, &durs);
    let mut sw = csv::Writer::from_path(&summary_path)?;
    for s in &summary {
        sw.serialize(s)?;
    }
    sw.flush()?;

    // Human-readable table to stdout.
    println!(
        "\nSovLoc simulation complete: {} raw rows\n",
        all_rows.len()
    );
    println!(
        "{:<10} {:>9} {:>8} {:>16} {:>16} {:>18}",
        "method", "part.dur", "success", "mean_loc(steps)", "±95% CI", "edge_success"
    );
    println!("{}", "-".repeat(82));
    for s in &summary {
        println!(
            "{:<10} {:>9} {:>7.0}% {:>16.1} {:>16.1} {:>17.0}%",
            s.method,
            s.partition_dur,
            s.success_rate * 100.0,
            s.mean_loc_latency,
            s.ci95_loc_latency,
            s.edge_culprit_success * 100.0
        );
    }
    println!(
        "\nWrote:\n  {}  (per-rep raw data)\n  {}  (aggregated summary)\n",
        raw_path, summary_path
    );
    println!("Reading: at partition_dur=0 both methods should be near-identical;");
    println!("as partition_dur grows, 'central' edge_success collapses while 'decentral' holds.");
    println!("1 timestep ≈ 100 ms. Numbers are simulated, validate the direction on real hardware with the mesh crate and the docker compose demo.");

    Ok(())
}
