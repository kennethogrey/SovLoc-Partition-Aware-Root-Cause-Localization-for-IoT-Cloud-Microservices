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
//!   * E5  network realism: packet loss, latency jitter, partial partitions, a
//!         flapping link, clock skew, and double faults, one impairment at a time.
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
    build_topology, count_false_alarms, simulate, simulate_scenario, FaultKind, Placement,
    RunParams, ScenarioRunParams, Side, TopoKind, STEP_MS,
};
use rca_core::Ranker;
use scenario::Scenario;

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
    /// Which experiments to run (comma separated: em,e1,e2,e4,e5,e7,fp). Default: all.
    #[arg(long, default_value = "em,e1,e2,e4,e5,e7,fp")]
    experiments: String,
    /// Scenario file for the EM multi-edge headline experiment.
    #[arg(long, default_value = "scenarios/five-edge-100.yaml")]
    scenario: String,
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

/// One raw per-run record for the EM multi-edge headline experiment. The device
/// id and its partition length are carried so the analysis can group the central
/// placement's time to diagnosis by the cut on each edge device.
#[derive(Serialize)]
struct EmRow {
    experiment: &'static str,
    scenario: String,
    placement: &'static str,
    ranker: &'static str,
    n_services: usize,
    n_devices: usize,
    culprit: String,
    culprit_side: &'static str,
    device: String,
    device_partition_sec: f64,
    part_offset: i64,
    loss: f64,
    seed: u64,
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
        Ranker::RandomWalk => "random-walk",
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
        culprit2: None,
        link: model::Link::default(),
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

/// E2: topologies x faults x {argmax, random-walk, ppr}, the detector never told
/// the onset. This is the graph ablation, so it runs on FULLY OBSERVED views
/// (no partition): every ranker sees the identical, complete telemetry for a given
/// seed (paired), and the only things that vary are the ranker and the
/// topology/fault structure. (Degraded observation under a partition is the subject
/// of E1/EM/E5, not of the ranker comparison; mixing it in would conflate ranking
/// skill with whether the collector could see the culprit at all.) argmax is
/// graph-blind, random-walk is the undirected graph baseline, PPR is ours; the
/// published BARO and CIRCA comparators run on the external RCAEval benchmark.
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
                    for ranker in [Ranker::Argmax, Ranker::RandomWalk, Ranker::Ppr] {
                        let mut p = base_params(seed, culprit);
                        p.kind = kind;
                        p.fault = fault;
                        p.n_extra = 5;
                        p.placement = Placement::EdgeLocal;
                        p.ranker = ranker;
                        p.part_len = 0; // fully observed: isolate the ranker
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

/// One raw per-run record for E5 (network realism). Each row varies a single
/// impairment (`condition`) to a `level`, holding the others ideal.
#[derive(Serialize)]
struct E5Row {
    experiment: &'static str,
    condition: &'static str,
    level: f64,
    placement: &'static str,
    ranker: &'static str,
    n_services: usize,
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

fn e5_row(condition: &'static str, level: f64, p: RunParams) -> E5Row {
    let topo = build_topology(p.kind, p.n_extra);
    let o = simulate(&p);
    E5Row {
        experiment: "e5",
        condition,
        level,
        placement: p.placement.as_str(),
        ranker: ranker_str(p.ranker),
        n_services: topo.graph.n,
        seed: p.seed,
        culprit: topo.graph.name(o.culprit).to_string(),
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
    }
}

/// E5: network realism. The store-and-forward central placement (the one most
/// exposed to the crossing link) under a single-impairment sweep: packet loss,
/// latency jitter, partial partitions, a flapping link, clock skew, and a
/// simultaneous second fault. An edge culprit so the telemetry must cross the
/// impaired link; a moderate partition so the during-cut impairments bite.
fn experiment_e5(base: u64, reps: usize) -> Vec<E5Row> {
    let mut rows = Vec::new();
    let services = build_topology(TopoKind::Chain, 5).graph.n; // 12
    #[allow(clippy::type_complexity)]
    let conds: Vec<(&'static str, Vec<f64>, Box<dyn Fn(f64, &mut RunParams)>)> = vec![
        (
            "loss",
            vec![0.0, 0.02, 0.05, 0.10],
            Box::new(|lv, p| p.loss = lv),
        ),
        (
            "jitter",
            vec![0.0, 5.0, 10.0],
            Box::new(|lv, p: &mut RunParams| p.link.jitter = lv as usize),
        ),
        (
            "partial",
            vec![1.0, 0.7, 0.4],
            Box::new(|lv, p| p.link.partial = lv),
        ),
        (
            "flapping",
            vec![0.0, 10.0, 20.0],
            Box::new(|lv, p| {
                p.link.flap_off = lv as usize;
                p.link.flap_on = if lv > 0.0 { 10 } else { 0 };
            }),
        ),
        (
            "clock_skew",
            vec![0.0, 5.0, 10.0],
            Box::new(|lv, p| p.link.skew = lv as i64),
        ),
        (
            "double_fault",
            vec![0.0, 1.0],
            Box::new(move |lv, p| {
                if lv > 0.0 {
                    let c = p.culprit.unwrap_or(0);
                    p.culprit2 = Some((c + 3) % services);
                }
            }),
        ),
    ];
    let mut cell = 0u64;
    for (name, levels, apply) in &conds {
        for &lv in levels {
            for culprit in 0..services {
                for rep in 0..reps {
                    let seed = base + cell * 10_000 + rep as u64;
                    let mut p = base_params(seed, culprit);
                    p.kind = TopoKind::Chain;
                    p.n_extra = 5;
                    p.placement = Placement::CentralSf;
                    p.part_len = 100;
                    apply(lv, &mut p);
                    rows.push(e5_row(name, lv, p));
                }
                cell += 1;
            }
        }
    }
    rows
}

/// EM: the multi-edge headline placement comparison. Loads a scenario (several
/// edge devices, each with its own microservices and partition length), runs the
/// four placements over every culprit and `reps` seeds, paired by seed so the
/// four placements see the same generated run. The partition start is randomised
/// relative to the fault (offsets in steps) as the examiner requires, and each
/// row carries the culprit's device and that device's partition length so the
/// analysis can plot time to diagnosis against the cut on each edge.
fn experiment_em(scenario_path: &str, base: u64, reps: usize) -> Result<Vec<EmRow>> {
    let scenario = Scenario::load(scenario_path)?;
    let st = scenario.build()?;
    let n = st.graph.n;
    let n_dev = st.device_ids.len();
    let offsets = [-10i64, 0, 10];
    let mut rows = Vec::new();
    for culprit in 0..n {
        for rep in 0..reps {
            let seed = base + (culprit as u64) * 100_000 + rep as u64;
            let offset = offsets[(seed % 3) as usize];
            for placement in Placement::all() {
                let p = ScenarioRunParams {
                    placement,
                    ranker: Ranker::Ppr,
                    fault: FaultKind::Latency,
                    part_offset: offset,
                    loss: 0.0,
                    sample_every: 1,
                    latency: 0,
                    bandwidth: 20,
                    h: 4.0,
                    persist: 3,
                    seed,
                    culprit: Some(culprit),
                    culprit2: None,
                    link: model::Link::default(),
                };
                let o = simulate_scenario(&st, &p);
                let device = match o.culprit_device {
                    Some(d) => st.device_ids[d].clone(),
                    None => "cloud".to_string(),
                };
                let dev_part_sec = o.culprit_partition_steps as f64 * STEP_MS / 1000.0;
                rows.push(EmRow {
                    experiment: "em",
                    scenario: scenario.name.clone(),
                    placement: placement.as_str(),
                    ranker: "ppr",
                    n_services: n,
                    n_devices: n_dev,
                    culprit: st.graph.name(o.culprit).to_string(),
                    culprit_side: if o.culprit_is_cloud { "cloud" } else { "edge" },
                    device,
                    device_partition_sec: dev_part_sec,
                    part_offset: offset,
                    loss: 0.0,
                    seed,
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
        }
    }
    Ok(rows)
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

/// Sum of the Intel RAPL energy counters (microjoules) across packages, or None
/// when RAPL is unreadable (not Intel, or no root). Reading `energy_uj` needs
/// privilege, so the energy experiment is run under sudo and is not in the default
/// set that `make reproduce` runs.
fn rapl_energy_uj() -> Option<u128> {
    let mut total = 0u128;
    let mut any = false;
    for i in 0..8 {
        let p = format!("/sys/class/powercap/intel-rapl/intel-rapl:{i}/energy_uj");
        if let Ok(s) = std::fs::read_to_string(&p) {
            if let Ok(v) = s.trim().parse::<u128>() {
                total += v;
                any = true;
            }
        }
    }
    any.then_some(total)
}

/// Energy (E4): measure the package energy drawn by a fixed localization workload
/// and report joules per diagnosis, both total and idle-subtracted (the marginal
/// energy attributable to the work above an idle baseline). Run under sudo so the
/// RAPL counter is readable:
///   sudo ./target/release/sim --experiments energy
fn experiment_energy(base: u64, runs: usize) {
    let services = build_topology(TopoKind::Chain, 5).graph.n;
    let run_one = |k: usize| -> bool {
        let mut p = base_params(base + k as u64, k % services);
        p.n_extra = 5;
        p.placement = Placement::EdgeLocal;
        p.part_len = 100;
        simulate(&p).diagnosed
    };

    // Idle baseline: package power over a short sleep, so we can subtract the draw
    // the machine would have spent doing nothing.
    let idle_secs = 2.0;
    let i0 = rapl_energy_uj();
    std::thread::sleep(std::time::Duration::from_secs_f64(idle_secs));
    let i1 = rapl_energy_uj();
    let idle_w = match (i0, i1) {
        (Some(a), Some(b)) => (b.wrapping_sub(a)) as f64 / 1e6 / idle_secs,
        _ => f64::NAN,
    };

    let before = rapl_energy_uj();
    let t0 = std::time::Instant::now();
    let mut diagnosed = 0usize;
    for k in 0..runs {
        if run_one(k) {
            diagnosed += 1;
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    match (before, rapl_energy_uj()) {
        (Some(a), Some(b)) => {
            let joules = b.wrapping_sub(a) as f64 / 1e6;
            let watts = joules / secs;
            let marginal = (joules - idle_w * secs).max(0.0);
            let per = |j: f64| {
                if diagnosed > 0 {
                    j / diagnosed as f64
                } else {
                    f64::NAN
                }
            };
            println!(
                "E4 energy: {runs} localizations in {secs:.2}s drew {joules:.3} J ({watts:.1} W; \
                 idle {idle_w:.1} W), {diagnosed} diagnosed. J/diagnosis: {:.6} total, \
                 {:.6} idle-subtracted",
                per(joules),
                per(marginal)
            );
            let _ = std::fs::create_dir_all("results/analysis");
            let _ = std::fs::write(
                "results/analysis/energy.txt",
                format!(
                    "runs={runs}\nseconds={secs:.3}\njoules={joules:.3}\nwatts={watts:.2}\n\
                     idle_watts={idle_w:.2}\nmarginal_joules={marginal:.3}\ndiagnosed={diagnosed}\n\
                     joules_per_diagnosis_total={:.6}\njoules_per_diagnosis_marginal={:.6}\n\
                     note=Package energy via Intel RAPL over the full localization pipeline \
                     (synthetic metric generation plus detector plus ranker); the marginal \
                     figure subtracts idle package draw and is the cost attributable to the work.\n",
                    per(joules),
                    per(marginal)
                ),
            );
        }
        _ => eprintln!(
            "E4 energy: RAPL not readable (need sudo and Intel RAPL). {runs} runs, \
             {diagnosed} diagnosed in {secs:.2}s."
        ),
    }
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
    let (rem, r1, r2, r4, r5, r7, rfp) = if cli.quick {
        (1, 2, 2, 1, 1, 1, 20)
    } else {
        (6, 10, 8, 3, 6, 4, 300)
    };

    if run("em") {
        let rows = experiment_em(&cli.scenario, cli.seed, rem)?;
        let path = format!("{}/raw_em.csv", cli.out_dir);
        write_rows(&path, &rows)?;
        println!(
            "EM multi-edge placement comparison ({}, {} services, {} edges): {} runs -> {}",
            cli.scenario,
            rows.first().map(|r| r.n_services).unwrap_or(0),
            rows.first().map(|r| r.n_devices).unwrap_or(0),
            rows.len(),
            path
        );
    }
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
    if run("e5") {
        let rows = experiment_e5(cli.seed, r5);
        let path = format!("{}/raw_e5.csv", cli.out_dir);
        write_rows(&path, &rows)?;
        println!("E5 network realism: {} runs -> {}", rows.len(), path);
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
    // Energy is opt-in (needs sudo for RAPL) and not in the default set.
    if run("energy") {
        experiment_energy(cli.seed, if cli.quick { 2000 } else { 50000 });
    }

    println!("\nDone. Run the `analysis` binary to produce the summary tables and plot data.");
    Ok(())
}
