//! SovLoc experiment driver.
//!
//! Runs the honest, transport-based model in [`model`] across the pre-registered
//! experiments and writes one raw per-run CSV per experiment under the output
//! directory. The companion `analysis` binary turns these into the summary
//! tables, the survival statistics, and the plot data, so that one command
//! (`make reproduce`) regenerates every number and figure from a clean checkout.
//!
//!   * E1  fair placement comparison: four placements, a swept partition, both
//!         edge-side and cloud-side faults, time to diagnosis as a censored
//!         quantity.
//!   * E2  detector and ranker validity: five topologies, four fault kinds,
//!         argmax vs personalized PageRank, the detector never told the onset.
//!   * E4  telemetry cost: bytes crossing the boundary vs sampling rate and
//!         service count, measured not assumed.
//!   * E7  scale: 7 to 100 services.
//!   * FP  false-alarm rate of the detector on fault-free data, h swept 3 to 8.
//!
//! Everything is seeded; a given seed reproduces a given run exactly.

mod model;

use std::fs;

use anyhow::Result;
use clap::Parser;
use serde::Serialize;

use model::{
    build_topology, count_false_alarms, simulate, FaultKind, Placement, RunParams, Side, TopoKind,
    STEP_MS,
};
use rca_core::Ranker;

/// Partition lengths in steps (one step is 100 ms), from no partition to about
/// ten minutes: at least eight non-zero levels as the review requires.
const PART_LEVELS: [usize; 9] = [0, 50, 100, 200, 400, 800, 1600, 3200, 6400];

#[derive(Parser, Debug)]
#[command(about = "SovLoc experiment driver (honest transport model)")]
struct Cli {
    /// Output directory for the raw CSVs.
    #[arg(long, default_value = "results")]
    out_dir: String,
    /// Base random seed.
    #[arg(long, default_value_t = 42)]
    seed: u64,
    /// Quick smoke run: fewer repetitions, for a fast sanity check.
    #[arg(long)]
    quick: bool,
    /// Which experiments to run (comma separated: e1,e2,e4,e7,fp). Default: all.
    #[arg(long, default_value = "e1,e2,e4,e7,fp")]
    experiments: String,
}

/// One raw per-run record. Optional columns are empty when they do not apply.
#[derive(Serialize)]
struct Row {
    experiment: &'static str,
    topology: &'static str,
    fault: &'static str,
    ranker: &'static str,
    placement: &'static str,
    n_services: usize,
    part_len: usize,
    part_sec: f64,
    part_offset: i64,
    loss: f64,
    sample_every: usize,
    h: f64,
    seed: u64,
    culprit: String,
    culprit_side: &'static str,
    diagnosed: bool,
    ttd_steps: Option<f64>,
    ttd_ms: Option<f64>,
    censored: bool,
    censored_at: Option<f64>,
    det_latency: Option<f64>,
    false_alarm: bool,
    final_rank: i64,
    cross_bytes: u64,
}

fn topo_str(k: TopoKind) -> &'static str {
    match k {
        TopoKind::Chain => "chain",
        TopoKind::Tree => "tree",
        TopoKind::Shared => "shared",
        TopoKind::Cycle => "cycle",
        TopoKind::Retry => "retry",
    }
}
fn fault_str(f: FaultKind) -> &'static str {
    match f {
        FaultKind::None => "none",
        FaultKind::Latency => "latency",
        FaultKind::Retry => "retry",
        FaultKind::Cpu => "cpu",
        FaultKind::Crash => "crash",
    }
}
fn ranker_str(r: Ranker) -> &'static str {
    match r {
        Ranker::Argmax => "argmax",
        Ranker::Ppr => "ppr",
    }
}

/// Run one placement on one fixed scenario and build its raw row.
fn push_run(rows: &mut Vec<Row>, experiment: &'static str, p: RunParams) {
    let topo = build_topology(p.kind, p.n_extra);
    let o = simulate(&p);
    let name = topo.graph.name(o.culprit).to_string();
    rows.push(Row {
        experiment,
        topology: topo_str(p.kind),
        fault: fault_str(p.fault),
        ranker: ranker_str(p.ranker),
        placement: p.placement.as_str(),
        n_services: topo.graph.n,
        part_len: p.part_len,
        part_sec: p.part_len as f64 * STEP_MS / 1000.0,
        part_offset: p.part_offset,
        loss: p.loss,
        sample_every: p.sample_every,
        h: p.h,
        seed: p.seed,
        culprit: name,
        culprit_side: if o.culprit_side == Side::Edge {
            "edge"
        } else {
            "cloud"
        },
        diagnosed: o.diagnosed,
        ttd_steps: o.diagnosed.then_some(o.ttd),
        ttd_ms: o.diagnosed.then_some(o.ttd * STEP_MS),
        censored: !o.diagnosed,
        censored_at: (!o.diagnosed).then_some(o.censored_at),
        det_latency: o.det_latency.is_finite().then_some(o.det_latency),
        false_alarm: o.false_alarm,
        final_rank: o.final_rank,
        cross_bytes: o.cross_bytes,
    });
}

fn base_params(seed: u64, culprit: usize) -> RunParams {
    RunParams {
        kind: TopoKind::Chain,
        fault: FaultKind::Latency,
        n_extra: 0,
        placement: Placement::CentralNaive,
        ranker: Ranker::Ppr,
        part_len: 0,
        part_offset: 0,
        loss: 0.0,
        sample_every: 1,
        latency: 0,
        bandwidth: 20,
        h: 4.0,
        persist: 3,
        seed,
        culprit: Some(culprit),
    }
}

/// E1: four placements, swept partition, both fault sides, PPR ranker.
fn experiment_e1(base: u64, reps: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    let services = build_topology(TopoKind::Chain, 0).graph.n; // 7
    let offsets = [-10i64, 0, 10];
    let mut cell = 0u64;
    for &pl in PART_LEVELS.iter() {
        for &loss in &[0.0, 0.1] {
            for culprit in 0..services {
                for rep in 0..reps {
                    let seed = base + cell * 10_000 + rep as u64;
                    let offset = offsets[(seed % 3) as usize];
                    for placement in Placement::all() {
                        let mut p = base_params(seed, culprit);
                        p.placement = placement;
                        p.part_len = pl;
                        p.part_offset = offset;
                        p.loss = loss;
                        push_run(&mut rows, "e1", p);
                    }
                }
                cell += 1;
            }
        }
    }
    rows
}

/// E2: topologies x faults x {argmax, ppr}, a fixed moderate partition, the
/// detector never told the onset. Placement is edge-local so the ranker is
/// compared on identical, fully observed views.
fn experiment_e2(base: u64, reps: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    let topos = [
        TopoKind::Chain,
        TopoKind::Tree,
        TopoKind::Shared,
        TopoKind::Cycle,
        TopoKind::Retry,
    ];
    let faults = [
        FaultKind::Latency,
        FaultKind::Retry,
        FaultKind::Cpu,
        FaultKind::Crash,
    ];
    let mut cell = 0u64;
    for &kind in &topos {
        let services = build_topology(kind, 5).graph.n;
        for &fault in &faults {
            for culprit in 0..services {
                for rep in 0..reps {
                    let seed = base + cell * 10_000 + rep as u64;
                    for ranker in [Ranker::Argmax, Ranker::Ppr] {
                        let mut p = base_params(seed, culprit);
                        p.kind = kind;
                        p.fault = fault;
                        p.n_extra = 5;
                        p.placement = Placement::EdgeLocal;
                        p.ranker = ranker;
                        p.part_len = 100;
                        push_run(&mut rows, "e2", p);
                    }
                    cell += 1;
                }
            }
        }
    }
    rows
}

/// E4: measured bytes vs sampling rate and service count.
fn experiment_e4(base: u64, reps: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    let sizes = [0usize, 5, 18, 43]; // 7, 12, 25, 50 services
    let samples = [1usize, 2, 10]; // 10 Hz, 5 Hz, 1 Hz at 100 ms steps
    let mut cell = 0u64;
    for &n_extra in &sizes {
        let services = build_topology(TopoKind::Chain, n_extra).graph.n;
        for &se in &samples {
            for culprit in 0..services {
                for rep in 0..reps {
                    let seed = base + cell * 10_000 + rep as u64;
                    for placement in Placement::all() {
                        let mut p = base_params(seed, culprit);
                        p.n_extra = n_extra;
                        p.sample_every = se;
                        p.placement = placement;
                        p.part_len = 200;
                        push_run(&mut rows, "e4", p);
                    }
                }
                cell += 1;
            }
        }
    }
    rows
}

/// E7: scale from 7 to 100 services.
fn experiment_e7(base: u64, reps: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    let sizes = [0usize, 5, 18, 43, 93]; // 7, 12, 25, 50, 100
    let mut cell = 0u64;
    for &n_extra in &sizes {
        let services = build_topology(TopoKind::Chain, n_extra).graph.n;
        for culprit in 0..services {
            for rep in 0..reps {
                let seed = base + cell * 10_000 + rep as u64;
                for placement in Placement::all() {
                    let mut p = base_params(seed, culprit);
                    p.n_extra = n_extra;
                    p.placement = placement;
                    p.part_len = 200;
                    push_run(&mut rows, "e7", p);
                }
            }
            cell += 1;
        }
    }
    rows
}

/// False-alarm rate of the detector on fault-free data, h swept 3 to 8.
#[derive(Serialize)]
struct FpRow {
    h: f64,
    persist: usize,
    runs: usize,
    horizon_steps: usize,
    observation_hours: f64,
    total_false_alarms: usize,
    false_alarms_per_hour: f64,
}

fn experiment_fp(base: u64, reps: usize) -> Vec<FpRow> {
    let mut rows = Vec::new();
    let horizon = 6000usize; // 600 s of fault-free data per run
    let persist = 3usize;
    for h_i in 3..=8 {
        let h = h_i as f64;
        let mut total = 0usize;
        for rep in 0..reps {
            let seed = base + (h_i as u64) * 100_000 + rep as u64;
            total += count_false_alarms(h, persist, horizon, seed);
        }
        // Seven services observed per run, so the observed service-time is reps *
        // horizon * 7 steps; report alarms per wall-clock hour of monitoring.
        let obs_hours = reps as f64 * horizon as f64 * STEP_MS / 1000.0 / 3600.0;
        rows.push(FpRow {
            h,
            persist,
            runs: reps,
            horizon_steps: horizon,
            observation_hours: obs_hours,
            total_false_alarms: total,
            false_alarms_per_hour: total as f64 / obs_hours,
        });
    }
    rows
}

fn write_rows<T: Serialize>(path: &str, rows: &[T]) -> Result<()> {
    let mut w = csv::Writer::from_path(path)?;
    for r in rows {
        w.serialize(r)?;
    }
    w.flush()?;
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    fs::create_dir_all(&cli.out_dir)?;
    let want: Vec<&str> = cli.experiments.split(',').map(|s| s.trim()).collect();
    let run = |name: &str| want.contains(&name);

    // Repetition counts (per cell). Quick mode shrinks them for a smoke test.
    let (r1, r2, r4, r7, rfp) = if cli.quick {
        (2, 2, 1, 1, 20)
    } else {
        (10, 8, 3, 4, 300)
    };

    if run("e1") {
        let rows = experiment_e1(cli.seed, r1);
        let path = format!("{}/raw_e1.csv", cli.out_dir);
        write_rows(&path, &rows)?;
        println!(
            "E1 fair placement comparison: {} runs -> {}",
            rows.len(),
            path
        );
    }
    if run("e2") {
        let rows = experiment_e2(cli.seed, r2);
        let path = format!("{}/raw_e2.csv", cli.out_dir);
        write_rows(&path, &rows)?;
        println!(
            "E2 detector and ranker ablation: {} runs -> {}",
            rows.len(),
            path
        );
    }
    if run("e4") {
        let rows = experiment_e4(cli.seed, r4);
        let path = format!("{}/raw_e4.csv", cli.out_dir);
        write_rows(&path, &rows)?;
        println!("E4 telemetry cost: {} runs -> {}", rows.len(), path);
    }
    if run("e7") {
        let rows = experiment_e7(cli.seed, r7);
        let path = format!("{}/raw_e7.csv", cli.out_dir);
        write_rows(&path, &rows)?;
        println!("E7 scale: {} runs -> {}", rows.len(), path);
    }
    if run("fp") {
        let rows = experiment_fp(cli.seed, rfp);
        let path = format!("{}/raw_fp.csv", cli.out_dir);
        write_rows(&path, &rows)?;
        println!("FP false-alarm sweep: {} rows -> {}", rows.len(), path);
    }

    println!("\nDone. Run the `analysis` binary to produce the summary tables and plot data.");
    Ok(())
}
