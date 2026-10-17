//! External-dataset benchmark for the SovLoc localizer.
//!
//! The simulator could be accused of embodying the detector's own assumptions, so
//! this binary runs the SAME detector and the SAME four rankers (argmax, BARO,
//! random-walk, PPR) on an independent benchmark with ground-truth root causes and
//! reports top-1 and top-3 accuracy per ranker. It targets the RCAEval datasets
//! (arXiv 2412.17015), which ship pre-collected metric time series and a labelled
//! root cause per fault case, so no live cluster is needed.
//!
//! A case directory holds:
//!   * `metrics.csv`  a header row then numeric rows. A leading `time`/`timestamp`
//!                    column is ignored. Columns named `<service>_<metric>` are
//!                    reduced to one series per service for the chosen `--metric`
//!                    (default `latency`); if none match, every column is a service.
//!   * `graph.csv`    optional `caller,callee[,weight]` edges (the call graph the
//!                    graph rankers walk). Absent: an edgeless graph, where the two
//!                    graph rankers fall back to the anomaly ranking.
//!   * `label.txt`    the ground-truth root-cause service name (first token), unless
//!                    given on the command line.
//!
//! Usage:
//!   rca-bench --dir <dataset-root> [--metric latency] [--out results/analysis/summary_ext.csv]
//! where <dataset-root> contains one subdirectory per case.

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use clap::Parser;

use rca_core::stats::robust_score;
use rca_core::{rank_causes, CallGraph, Ranker, RcaConfig};

const RANKERS: [(&str, Ranker); 4] = [
    ("argmax", Ranker::Argmax),
    ("baro", Ranker::Baro),
    ("random-walk", Ranker::RandomWalk),
    ("ppr", Ranker::Ppr),
];

#[derive(Parser, Debug)]
#[command(about = "Run the SovLoc rankers on an external RCA dataset")]
struct Cli {
    /// Dataset root holding one subdirectory per fault case.
    #[arg(long)]
    dir: String,
    /// Which per-service metric to localize on (column suffix `_<metric>`).
    #[arg(long, default_value = "latency")]
    metric: String,
    /// Output summary CSV.
    #[arg(long, default_value = "results/analysis/summary_ext.csv")]
    out: String,
}

/// Parse a metrics CSV into service names and `series[service][t]`. A leading
/// time column is dropped. Columns `<service>_<metric>` collapse to one series per
/// service; if no column carries the suffix, each column is treated as a service.
fn parse_metrics(text: &str, metric: &str) -> Result<(Vec<String>, Vec<Vec<f64>>)> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().context("metrics.csv is empty")?;
    let cols: Vec<&str> = header.split(',').map(|s| s.trim()).collect();

    let is_time = |c: &str| {
        let c = c.to_ascii_lowercase();
        c == "time" || c == "timestamp" || c == "ts" || c.is_empty()
    };
    let suffix = format!("_{metric}");
    let has_suffix = cols.iter().any(|c| c.ends_with(&suffix));

    // Map each kept column index to a service name.
    let mut keep: Vec<(usize, String)> = Vec::new();
    for (i, c) in cols.iter().enumerate() {
        if is_time(c) {
            continue;
        }
        if has_suffix {
            if let Some(svc) = c.strip_suffix(&suffix) {
                keep.push((i, svc.to_string()));
            }
        } else {
            keep.push((i, c.to_string()));
        }
    }
    if keep.is_empty() {
        bail!("no service columns found (metric suffix _{metric})");
    }

    let names: Vec<String> = keep.iter().map(|(_, n)| n.clone()).collect();
    let mut series: Vec<Vec<f64>> = vec![Vec::new(); names.len()];
    for line in lines {
        let fields: Vec<&str> = line.split(',').collect();
        for (si, (ci, _)) in keep.iter().enumerate() {
            let v = fields.get(*ci).and_then(|x| x.trim().parse::<f64>().ok());
            series[si].push(v.unwrap_or(f64::NAN));
        }
    }
    Ok((names, series))
}

/// Parse a `caller,callee[,weight]` edge list into a call graph over `names`.
/// Unknown service names are ignored so a graph can name a subset.
fn parse_graph(text: &str, names: &[String]) -> CallGraph {
    let mut g = CallGraph::new(names.to_vec());
    let idx = |n: &str| names.iter().position(|x| x == n);
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let f: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if f.len() < 2 || f[0].eq_ignore_ascii_case("caller") {
            continue; // skip a header or malformed row
        }
        if let (Some(a), Some(b)) = (idx(f[0]), idx(f[1])) {
            let w = f.get(2).and_then(|x| x.parse::<f64>().ok()).unwrap_or(1.0);
            g.add_edge(a, b, w);
        }
    }
    g
}

/// One case's result: per ranker, its name and whether the culprit was top-1/top-3.
type CaseScore = Vec<(&'static str, bool, bool)>;

/// Score one case with every ranker. Returns `(ranker_name, top1, top3)` per
/// ranker. The anomaly is the robust deviation of each service's last sample from
/// its own pre-fault baseline, exactly as the simulator and the mesh compute it.
fn score_case(
    names: &[String],
    series: &[Vec<f64>],
    graph: &CallGraph,
    culprit: usize,
) -> CaseScore {
    let n = names.len();
    let t = series.iter().map(|s| s.len()).min().unwrap_or(0);
    let baseline_len = (t / 3).clamp(5, 40).min(t.saturating_sub(1).max(1));
    let cfg = RcaConfig::default();

    let mut anomaly = vec![0.0_f64; n];
    let mut observed = vec![false; n];
    for i in 0..n {
        if series[i].len() > baseline_len {
            let base: Vec<f64> = series[i][..baseline_len]
                .iter()
                .copied()
                .filter(|x| x.is_finite())
                .collect();
            let last = series[i][t - 1];
            if base.len() >= 3 && last.is_finite() {
                anomaly[i] = robust_score(&base, last);
                observed[i] = true;
            }
        }
    }

    RANKERS
        .iter()
        .map(|(name, r)| {
            let ranked = rank_causes(graph, &anomaly, &observed, &cfg, *r);
            let top1 = ranked.first().map(|c| c.service) == Some(culprit);
            let top3 = ranked.iter().take(3).any(|c| c.service == culprit);
            (*name, top1, top3)
        })
        .collect()
}

/// Load one case directory and score it; None when it is not a usable case.
fn run_case(dir: &Path, metric: &str, cli_culprit: Option<&str>) -> Result<Option<CaseScore>> {
    let mpath = dir.join("metrics.csv");
    if !mpath.exists() {
        return Ok(None);
    }
    let (names, series) = parse_metrics(&fs::read_to_string(&mpath)?, metric)?;
    let graph = match fs::read_to_string(dir.join("graph.csv")) {
        Ok(text) => parse_graph(&text, &names),
        Err(_) => CallGraph::new(names.clone()),
    };
    let label = match cli_culprit {
        Some(c) => c.to_string(),
        None => fs::read_to_string(dir.join("label.txt"))
            .map(|s| s.split_whitespace().next().unwrap_or("").to_string())
            .unwrap_or_default(),
    };
    let Some(culprit) = names.iter().position(|n| n == &label) else {
        eprintln!(
            "skip {}: root cause '{label}' not among services",
            dir.display()
        );
        return Ok(None);
    };
    Ok(Some(score_case(&names, &series, &graph, culprit)))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = Path::new(&cli.dir);
    if !root.is_dir() {
        bail!("dataset root {} is not a directory", cli.dir);
    }

    // Tally top-1 and top-3 hits per ranker across cases.
    let mut top1 = [0usize; RANKERS.len()];
    let mut top3 = [0usize; RANKERS.len()];
    let mut n_cases = 0usize;

    let mut entries: Vec<_> = fs::read_dir(root)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.path());
    for e in &entries {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if let Some(scores) = run_case(&p, &cli.metric, None)? {
            n_cases += 1;
            for (i, (_, a, b)) in scores.iter().enumerate() {
                if *a {
                    top1[i] += 1;
                }
                if *b {
                    top3[i] += 1;
                }
            }
        }
    }

    if n_cases == 0 {
        bail!(
            "no usable cases under {} (each case needs metrics.csv + a label)",
            cli.dir
        );
    }

    if let Some(parent) = Path::new(&cli.out).parent() {
        fs::create_dir_all(parent)?;
    }
    let mut w = csv::Writer::from_path(&cli.out)?;
    w.write_record(["ranker", "cases", "top1_pct", "top3_pct"])?;
    println!(
        "External benchmark ({n_cases} cases, metric {}):",
        cli.metric
    );
    for (i, (name, _)) in RANKERS.iter().enumerate() {
        let t1 = 100.0 * top1[i] as f64 / n_cases as f64;
        let t3 = 100.0 * top3[i] as f64 / n_cases as f64;
        w.write_record([
            name.to_string(),
            n_cases.to_string(),
            format!("{t1:.1}"),
            format!("{t3:.1}"),
        ])?;
        println!("  {name:<12} top-1 {t1:5.1}%   top-3 {t3:5.1}%");
    }
    w.flush()?;
    println!("Wrote {}", cli.out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny three-service case: gateway -> api -> db. The db (deepest) is the
    // culprit; its caller api is amplified above it, so only the graph-aware PPR
    // walk should put db first, while argmax/BARO pick the amplified api.
    const METRICS: &str = "\
time,gateway_latency,api_latency,db_latency
0,20,20,20
1,21,19,20
2,20,20,21
3,20,21,19
4,20,20,20
5,20,19,21
6,22,40,30
7,23,55,42
8,24,60,45
9,24,62,46
10,25,63,47
11,25,64,48
";
    const GRAPH: &str = "caller,callee\ngateway,api\napi,db\n";

    #[test]
    fn parse_metrics_collapses_service_columns() {
        let (names, series) = parse_metrics(METRICS, "latency").unwrap();
        assert_eq!(names, vec!["gateway", "api", "db"]);
        assert_eq!(series[0].len(), 12);
        assert_eq!(series[2][0], 20.0);
    }

    #[test]
    fn ppr_wins_on_the_amplified_caller_case() {
        let (names, series) = parse_metrics(METRICS, "latency").unwrap();
        let graph = parse_graph(GRAPH, &names);
        let culprit = names.iter().position(|n| n == "db").unwrap();
        let scores = score_case(&names, &series, &graph, culprit);
        let by = |nm: &str| scores.iter().find(|(n, _, _)| *n == nm).unwrap();
        assert!(by("ppr").1, "PPR should get top-1 on the deepest culprit");
        assert!(
            !by("argmax").1,
            "argmax should miss top-1 (picks the amplified caller)"
        );
        // Everyone should at least rank the culprit in the top 3 of 3.
        assert!(by("ppr").2 && by("argmax").2);
    }
}
