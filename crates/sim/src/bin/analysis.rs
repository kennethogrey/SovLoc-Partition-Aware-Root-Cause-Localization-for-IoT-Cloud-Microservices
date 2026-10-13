//! SovLoc analysis: turn the raw per-run CSVs into the summary tables, the
//! survival statistics, the plot data, and a `numbers.tex` of LaTeX macros so the
//! manuscript cites values straight from the runs. One command (`make reproduce`)
//! runs the `sim` driver and then this binary, regenerating every number and
//! figure from a clean checkout.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;

use anyhow::{Context, Result};
use serde::Deserialize;

use rca_core::stats::quantile;
use rca_core::survival::{
    bootstrap_ci, holm, kaplan_meier, logrank, mean, tost_means, wilson_interval, Censored,
    XorShift64,
};

const Z: f64 = 1.96;
const N_BOOT: usize = 2000;
const BOOT_SEED: u64 = 12345;

#[derive(Deserialize, Clone)]
struct Raw {
    #[allow(dead_code)]
    experiment: String,
    topology: String,
    fault: String,
    ranker: String,
    placement: String,
    n_services: usize,
    part_len: usize,
    #[allow(dead_code)]
    part_sec: f64,
    #[allow(dead_code)]
    part_offset: i64,
    #[allow(dead_code)]
    loss: f64,
    sample_every: usize,
    #[allow(dead_code)]
    h: f64,
    seed: u64,
    #[allow(dead_code)]
    culprit: String,
    culprit_side: String,
    diagnosed: bool,
    ttd_steps: Option<f64>,
    #[allow(dead_code)]
    ttd_ms: Option<f64>,
    #[allow(dead_code)]
    censored: bool,
    censored_at: Option<f64>,
    #[allow(dead_code)]
    det_latency: Option<f64>,
    #[allow(dead_code)]
    false_alarm: bool,
    final_rank: i64,
    cross_bytes: u64,
}

fn read(path: &str) -> Result<Vec<Raw>> {
    let mut rdr = csv::Reader::from_path(path).with_context(|| format!("open {path}"))?;
    let mut out = Vec::new();
    for rec in rdr.deserialize() {
        out.push(rec?);
    }
    Ok(out)
}

fn censored_of(r: &Raw) -> Censored {
    if r.diagnosed {
        Censored {
            time: r.ttd_steps.unwrap_or(0.0),
            event: true,
        }
    } else {
        Censored {
            time: r.censored_at.unwrap_or(0.0),
            event: false,
        }
    }
}

/// Kaplan-Meier quantile (time at which survival first drops to or below `1-q`).
/// Returns None when that quantile is not reached (too many censored).
fn km_quantile(data: &[Censored], q: f64) -> Option<f64> {
    let (curve, _median) = kaplan_meier(data);
    let target = 1.0 - q;
    for (t, s) in curve {
        if s <= target + 1e-12 {
            return Some(t);
        }
    }
    None
}

fn pct(x: f64) -> f64 {
    x * 100.0
}

/// Least-squares slope of y on x.
fn slope(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len() as f64;
    if n < 2.0 {
        return f64::NAN;
    }
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for i in 0..xs.len() {
        num += (xs[i] - mx) * (ys[i] - my);
        den += (xs[i] - mx) * (xs[i] - mx);
    }
    if den == 0.0 {
        f64::NAN
    } else {
        num / den
    }
}

/// Two-sided bootstrap p-value and 95% CI for a slope of y on x (resampling
/// paired points).
fn slope_boot(xs: &[f64], ys: &[f64], seed: u64) -> (f64, f64, f64, f64) {
    let b = slope(xs, ys);
    let mut rng = XorShift64::new(seed);
    let mut boots = Vec::with_capacity(N_BOOT);
    let (mut sx, mut sy) = (vec![0.0; xs.len()], vec![0.0; xs.len()]);
    for _ in 0..N_BOOT {
        for k in 0..xs.len() {
            let idx = rng.below(xs.len());
            sx[k] = xs[idx];
            sy[k] = ys[idx];
        }
        boots.push(slope(&sx, &sy));
    }
    boots.sort_by(|a, c| a.partial_cmp(c).unwrap_or(std::cmp::Ordering::Equal));
    let lo = boots[(0.025 * N_BOOT as f64) as usize];
    let hi = boots[((0.975 * N_BOOT as f64) as usize).min(N_BOOT - 1)];
    let below = boots.iter().filter(|&&v| v <= 0.0).count() as f64 / N_BOOT as f64;
    let p = 2.0 * below.min(1.0 - below);
    (b, lo, hi, p)
}

fn main() -> Result<()> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "results".into());
    let out = format!("{dir}/analysis");
    fs::create_dir_all(format!("{out}/figdata"))?;

    let e1 = read(&format!("{dir}/raw_e1.csv"))?;
    let e2 = read(&format!("{dir}/raw_e2.csv"))?;
    let e4 = read(&format!("{dir}/raw_e4.csv"))?;
    let e7 = read(&format!("{dir}/raw_e7.csv"))?;

    let mut numbers = String::new();
    let mut stats = String::new();

    // ---- E1 summary table (the headline results) -----------------------------
    let placements = ["central-naive", "central-sf", "edge-local", "hybrid"];
    let mut parts: Vec<usize> = e1.iter().map(|r| r.part_len).collect();
    parts.sort_unstable();
    parts.dedup();

    let mut e1w = csv::Writer::from_path(format!("{out}/summary_e1.csv"))?;
    e1w.write_record([
        "placement",
        "part_len",
        "part_sec",
        "n",
        "success_pct",
        "success_lo",
        "success_hi",
        "edge_success_pct",
        "edge_lo",
        "edge_hi",
        "cloud_success_pct",
        "cloud_lo",
        "cloud_hi",
        "edge_median_ttd",
        "cloud_median_ttd",
        "median_ttd_steps",
        "iqr_q25_steps",
        "iqr_q75_steps",
        "p95_ttd_steps",
        "p99_ttd_steps",
        "mean_cross_kb",
    ])?;
    // Supplementary ECDF (censoring aware: CDF = 1 - Kaplan-Meier survival) for
    // every E1 cell, the per-cell distribution the examiner asks for in a CSV.
    let mut ecdfw = csv::Writer::from_path(format!("{out}/ecdf_e1.csv"))?;
    ecdfw.write_record(["placement", "part_len", "ttd_steps", "ecdf"])?;

    // plot data files
    let mut f_ttd = fs::File::create(format!("{out}/figdata/e1_edge_median_ttd.dat"))?;
    let mut f_cttd = fs::File::create(format!("{out}/figdata/e1_cloud_median_ttd.dat"))?;
    let mut f_edge = fs::File::create(format!("{out}/figdata/e1_edge_success.dat"))?;
    let mut f_cloud = fs::File::create(format!("{out}/figdata/e1_cloud_success.dat"))?;
    writeln!(f_ttd, "part_sec central_naive central_sf edge_local hybrid")?;
    writeln!(
        f_cttd,
        "part_sec central_naive central_sf edge_local hybrid"
    )?;
    writeln!(
        f_edge,
        "part_sec central_naive central_sf edge_local hybrid"
    )?;
    writeln!(
        f_cloud,
        "part_sec central_naive central_sf edge_local hybrid"
    )?;

    // Collect per (placement, part) stats and the plot columns.
    let mut edge_ttd_col: BTreeMap<(String, usize), f64> = BTreeMap::new();
    let mut cloud_ttd_col: BTreeMap<(String, usize), f64> = BTreeMap::new();
    let mut edge_succ_col: BTreeMap<(String, usize), f64> = BTreeMap::new();
    let mut cloud_succ_col: BTreeMap<(String, usize), f64> = BTreeMap::new();

    for &pl in &placements {
        for &pt in &parts {
            let grp: Vec<&Raw> = e1
                .iter()
                .filter(|r| r.placement == pl && r.part_len == pt)
                .collect();
            if grp.is_empty() {
                continue;
            }
            let n = grp.len();
            let diag = grp.iter().filter(|r| r.diagnosed).count();
            let (sp, slo, shi) = wilson_interval(diag, n, Z);

            let edge: Vec<&&Raw> = grp.iter().filter(|r| r.culprit_side == "edge").collect();
            let cloud: Vec<&&Raw> = grp.iter().filter(|r| r.culprit_side == "cloud").collect();
            let ediag = edge.iter().filter(|r| r.diagnosed).count();
            let cdiag = cloud.iter().filter(|r| r.diagnosed).count();
            let (ep, elo, ehi) = wilson_interval(ediag, edge.len().max(1), Z);
            let (cp, clo, chi) = wilson_interval(cdiag, cloud.len().max(1), Z);

            let cens: Vec<Censored> = grp.iter().map(|r| censored_of(r)).collect();
            let (km_curve, km_med) = kaplan_meier(&cens);
            let q25 = km_quantile(&cens, 0.25).unwrap_or(f64::NAN);
            let q75 = km_quantile(&cens, 0.75).unwrap_or(f64::NAN);
            let p95 = km_quantile(&cens, 0.95).unwrap_or(f64::NAN);
            let p99 = km_quantile(&cens, 0.99).unwrap_or(f64::NAN);
            let mean_kb = grp.iter().map(|r| r.cross_bytes as f64).sum::<f64>() / n as f64 / 1024.0;

            // Per-cell ECDF: CDF = 1 - S(t) at each Kaplan-Meier step.
            for (t, s) in &km_curve {
                ecdfw.write_record([
                    pl.to_string(),
                    pt.to_string(),
                    format!("{t:.1}"),
                    format!("{:.4}", 1.0 - s),
                ])?;
            }

            // Per-side median time to diagnosis (the symmetric story: central
            // grows for edge faults, edge-local grows for cloud faults).
            let edge_cens: Vec<Censored> = edge.iter().map(|r| censored_of(r)).collect();
            let cloud_cens: Vec<Censored> = cloud.iter().map(|r| censored_of(r)).collect();
            let (_e, emed) = kaplan_meier(&edge_cens);
            let (_cl, cmed) = kaplan_meier(&cloud_cens);
            edge_ttd_col.insert((pl.to_string(), pt), emed);
            cloud_ttd_col.insert((pl.to_string(), pt), cmed);
            edge_succ_col.insert((pl.to_string(), pt), pct(ep));
            cloud_succ_col.insert((pl.to_string(), pt), pct(cp));

            e1w.write_record([
                pl.to_string(),
                pt.to_string(),
                format!("{:.1}", pt as f64 * 0.1),
                n.to_string(),
                format!("{:.1}", pct(sp)),
                format!("{:.1}", pct(slo)),
                format!("{:.1}", pct(shi)),
                format!("{:.1}", pct(ep)),
                format!("{:.1}", pct(elo)),
                format!("{:.1}", pct(ehi)),
                format!("{:.1}", pct(cp)),
                format!("{:.1}", pct(clo)),
                format!("{:.1}", pct(chi)),
                fmt_opt(emed),
                fmt_opt(cmed),
                fmt_opt(km_med),
                fmt_opt(q25),
                fmt_opt(q75),
                fmt_opt(p95),
                fmt_opt(p99),
                format!("{mean_kb:.2}"),
            ])?;
        }
    }
    e1w.flush()?;
    ecdfw.flush()?;

    // Write the plot columns (one row per partition).
    for &pt in &parts {
        let sec = pt as f64 * 0.1;
        let col = |m: &BTreeMap<(String, usize), f64>| {
            placements
                .iter()
                .map(|p| {
                    let v = m.get(&(p.to_string(), pt)).copied().unwrap_or(f64::NAN);
                    if v.is_nan() {
                        "nan".to_string()
                    } else {
                        format!("{v:.2}")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        writeln!(f_ttd, "{sec:.1} {}", col(&edge_ttd_col))?;
        writeln!(f_cttd, "{sec:.1} {}", col(&cloud_ttd_col))?;
        writeln!(f_edge, "{sec:.1} {}", col(&edge_succ_col))?;
        writeln!(f_cloud, "{sec:.1} {}", col(&cloud_succ_col))?;
    }

    // Self-contained results figure (no TeX or gnuplot needed by the one command):
    // median time to diagnosis against partition length, edge-side and cloud-side
    // faults, on a log y axis and a proportional x axis, censored medians drawn as
    // hollow markers at the ceiling.
    let svg = results_svg(&placements, &parts, &edge_ttd_col, &cloud_ttd_col);
    fs::write(format!("{out}/fig_sovloc_results.svg"), svg)?;

    // ---- Headline numbers for the manuscript ---------------------------------
    let longest = *parts.last().unwrap();
    // control at partition 0: edge-local vs central-sf accuracy (TOST, margin 0.05)
    let pair_at = |pl: &str, pt: usize| -> Vec<(u64, f64)> {
        e1.iter()
            .filter(|r| r.placement == pl && r.part_len == pt && r.culprit_side == "edge")
            .map(|r| (r.seed, if r.diagnosed { 1.0 } else { 0.0 }))
            .collect()
    };
    let (edge0, sf0) = (pair_at("edge-local", 0), pair_at("central-sf", 0));
    let edge0_v: Vec<f64> = edge0.iter().map(|x| x.1).collect();
    let sf0_v: Vec<f64> = sf0.iter().map(|x| x.1).collect();
    let (tost_diff, tost_pl, tost_pu) = tost_means(&edge0_v, &sf0_v, 0.05);
    let tost_p = tost_pl.max(tost_pu);

    // C4: edge-local vs central-sf edge-fault censored TtD at the longest partition.
    let edge_cens_el: Vec<Censored> = e1
        .iter()
        .filter(|r| {
            r.placement == "edge-local" && r.part_len == longest && r.culprit_side == "edge"
        })
        .map(censored_of)
        .collect();
    let edge_cens_sf: Vec<Censored> = e1
        .iter()
        .filter(|r| {
            r.placement == "central-sf" && r.part_len == longest && r.culprit_side == "edge"
        })
        .map(censored_of)
        .collect();
    let (lr_chi, lr_p) = logrank(&edge_cens_el, &edge_cens_sf);

    // C1/C2: slope of edge-fault TtD on partition length (diagnosed runs only).
    let slope_pts = |pl: &str| -> (Vec<f64>, Vec<f64>) {
        let mut xs = Vec::new();
        let mut ys = Vec::new();
        for r in e1
            .iter()
            .filter(|r| r.placement == pl && r.culprit_side == "edge" && r.diagnosed)
        {
            xs.push(r.part_len as f64);
            ys.push(r.ttd_steps.unwrap_or(0.0));
        }
        (xs, ys)
    };
    let (sx, sy) = slope_pts("central-sf");
    let (sf_slope, sf_slo, sf_shi, sf_sp) = slope_boot(&sx, &sy, BOOT_SEED);
    let (ex, ey) = slope_pts("edge-local");
    let (el_slope, el_slo, el_shi, el_sp) = slope_boot(&ex, &ey, BOOT_SEED + 1);

    // ---- E2 ablation: PPR vs argmax top-1 accuracy, paired by seed -----------
    let mut e2w = csv::Writer::from_path(format!("{out}/summary_e2.csv"))?;
    e2w.write_record([
        "topology",
        "fault",
        "n",
        "argmax_top1_pct",
        "ppr_top1_pct",
        "ppr_minus_argmax_pct",
    ])?;
    let mut topos: Vec<String> = e2.iter().map(|r| r.topology.clone()).collect();
    topos.sort();
    topos.dedup();
    let mut faults: Vec<String> = e2.iter().map(|r| r.fault.clone()).collect();
    faults.sort();
    faults.dedup();
    // paired PPR-argmax differences pooled for the headline contrast C5
    let mut ppr_minus_argmax: Vec<f64> = Vec::new();
    for tp in &topos {
        for ft in &faults {
            // pair by (seed, culprit): one argmax and one ppr run
            let mut amap: BTreeMap<(u64, String), bool> = BTreeMap::new();
            let mut pmap: BTreeMap<(u64, String), bool> = BTreeMap::new();
            for r in e2.iter().filter(|r| &r.topology == tp && &r.fault == ft) {
                let key = (r.seed, r.culprit.clone());
                let hit = r.final_rank == 1;
                if r.ranker == "argmax" {
                    amap.insert(key, hit);
                } else {
                    pmap.insert(key, hit);
                }
            }
            let mut a_hits = 0usize;
            let mut p_hits = 0usize;
            let mut n = 0usize;
            for (k, &ah) in &amap {
                if let Some(&ph) = pmap.get(k) {
                    n += 1;
                    if ah {
                        a_hits += 1;
                    }
                    if ph {
                        p_hits += 1;
                    }
                    ppr_minus_argmax.push((ph as i32 - ah as i32) as f64);
                }
            }
            if n == 0 {
                continue;
            }
            let ap = pct(a_hits as f64 / n as f64);
            let pp = pct(p_hits as f64 / n as f64);
            e2w.write_record([
                tp.clone(),
                ft.clone(),
                n.to_string(),
                format!("{ap:.1}"),
                format!("{pp:.1}"),
                format!("{:.1}", pp - ap),
            ])?;
        }
    }
    e2w.flush()?;
    let (c5_diff, c5_lo, c5_hi) =
        bootstrap_ci(&ppr_minus_argmax, mean, N_BOOT, 0.025, BOOT_SEED + 2);
    // p-value for C5 via the bootstrap (two-sided).
    let c5_p = {
        let mut rng = XorShift64::new(BOOT_SEED + 3);
        let mut below = 0usize;
        let mut s = vec![0.0; ppr_minus_argmax.len()];
        for _ in 0..N_BOOT {
            for x in s.iter_mut() {
                *x = ppr_minus_argmax[rng.below(ppr_minus_argmax.len())];
            }
            if mean(&s) <= 0.0 {
                below += 1;
            }
        }
        let f = below as f64 / N_BOOT as f64;
        2.0 * f.min(1.0 - f)
    };

    // ---- Holm over the difference-test family (C1, C4, C5) -------------------
    let fam = [sf_sp, lr_p, c5_p];
    let holm_adj = holm(&fam, 0.05);

    // ---- E4 telemetry cost: mean cross KB per (placement, services, rate) ----
    let mut e4w = csv::Writer::from_path(format!("{out}/summary_e4.csv"))?;
    e4w.write_record([
        "placement",
        "n_services",
        "sample_every_hz",
        "mean_cross_kb",
    ])?;
    let mut e4keys: Vec<(String, usize, usize)> = e4
        .iter()
        .map(|r| (r.placement.clone(), r.n_services, r.sample_every))
        .collect();
    e4keys.sort();
    e4keys.dedup();
    for (pl, ns, se) in &e4keys {
        let grp: Vec<&Raw> = e4
            .iter()
            .filter(|r| &r.placement == pl && r.n_services == *ns && r.sample_every == *se)
            .collect();
        let kb = grp.iter().map(|r| r.cross_bytes as f64).sum::<f64>() / grp.len() as f64 / 1024.0;
        let hz = 10.0 / *se as f64; // 100 ms steps -> 10 Hz at sample_every=1
        e4w.write_record([
            pl.clone(),
            ns.to_string(),
            format!("{hz:.0}"),
            format!("{kb:.2}"),
        ])?;
    }
    e4w.flush()?;

    // ---- E7 scale: median TtD + success + bytes per (placement, services) ----
    let mut e7w = csv::Writer::from_path(format!("{out}/summary_e7.csv"))?;
    e7w.write_record([
        "placement",
        "n_services",
        "success_pct",
        "median_ttd_steps",
        "mean_cross_kb",
    ])?;
    let mut e7keys: Vec<(String, usize)> = e7
        .iter()
        .map(|r| (r.placement.clone(), r.n_services))
        .collect();
    e7keys.sort();
    e7keys.dedup();
    for (pl, ns) in &e7keys {
        let grp: Vec<&Raw> = e7
            .iter()
            .filter(|r| &r.placement == pl && r.n_services == *ns)
            .collect();
        let diag = grp.iter().filter(|r| r.diagnosed).count();
        let (sp, _, _) = wilson_interval(diag, grp.len(), Z);
        let cens: Vec<Censored> = grp.iter().map(|r| censored_of(r)).collect();
        let (_c, med) = kaplan_meier(&cens);
        let kb = grp.iter().map(|r| r.cross_bytes as f64).sum::<f64>() / grp.len() as f64 / 1024.0;
        e7w.write_record([
            pl.clone(),
            ns.to_string(),
            format!("{:.1}", pct(sp)),
            fmt_opt(med),
            format!("{kb:.2}"),
        ])?;
    }
    e7w.flush()?;

    // ---- bytes ratio for H3 (edge-local vs central-sf at base size, 10 Hz) ---
    let bytes_of = |pl: &str| -> f64 {
        let g: Vec<&Raw> = e4
            .iter()
            .filter(|r| r.placement == pl && r.n_services == 7 && r.sample_every == 1)
            .collect();
        g.iter().map(|r| r.cross_bytes as f64).sum::<f64>() / g.len().max(1) as f64
    };
    let sf_bytes = bytes_of("central-sf");
    let el_bytes = bytes_of("edge-local");
    let hy_bytes = bytes_of("hybrid");
    let ratio_el = sf_bytes / el_bytes.max(1.0);
    let ratio_hy = sf_bytes / hy_bytes.max(1.0);

    // ---- stats.txt -----------------------------------------------------------
    stats.push_str("SovLoc statistics report\n========================\n\n");
    stats.push_str(&format!(
        "C1 central-sf edge-fault TtD vs partition length: slope {:.4} steps/step \
         (95% CI {:.4}..{:.4}), two-sided p={:.4}\n",
        sf_slope, sf_slo, sf_shi, sf_sp
    ));
    stats.push_str(&format!(
        "C2 edge-local edge-fault TtD vs partition length: slope {:.4} steps/step \
         (95% CI {:.4}..{:.4}), p={:.4} (expected to include 0)\n",
        el_slope, el_slo, el_shi, el_sp
    ));
    stats.push_str(&format!(
        "C3 TOST edge-local vs central-sf top-1 at partition 0 (margin 0.05): \
         diff={:.3}, p_lower={:.4}, p_upper={:.4}, equivalent={}\n",
        tost_diff,
        tost_pl,
        tost_pu,
        tost_p < 0.05
    ));
    stats.push_str(&format!(
        "C4 log-rank edge-local vs central-sf edge-fault TtD at longest partition \
         ({} steps): chi2={:.2}, p={:.4}\n",
        longest, lr_chi, lr_p
    ));
    stats.push_str(&format!(
        "C5 PPR minus argmax top-1 (pooled, paired): {:.3} \
         (95% CI {:.3}..{:.3}), p={:.4}\n",
        c5_diff, c5_lo, c5_hi, c5_p
    ));
    stats.push_str("\nHolm correction over the difference-test family (C1, C4, C5):\n");
    for (i, (name, p)) in [("C1", sf_sp), ("C4", lr_p), ("C5", c5_p)]
        .iter()
        .enumerate()
    {
        stats.push_str(&format!(
            "  {name}: raw p={:.4}, Holm-adjusted p={:.4}, reject at 0.05: {}\n",
            p, holm_adj[i].0, holm_adj[i].1
        ));
    }
    stats.push_str(&format!(
        "\nH3 bytes crossing per run (base size, 10 Hz): central-sf {:.0}, edge-local {:.0}, \
         hybrid {:.0}; central-sf/edge-local = {:.1}x, central-sf/hybrid = {:.0}x\n",
        sf_bytes, el_bytes, hy_bytes, ratio_el, ratio_hy
    ));
    fs::write(format!("{out}/stats.txt"), &stats)?;

    // ---- numbers.tex ---------------------------------------------------------
    let m = |name: &str, val: String| format!("\\newcommand{{\\{name}}}{{{val}}}\n");
    numbers.push_str("% Auto-generated by the analysis binary. Do not edit by hand.\n");
    numbers.push_str(&m(
        "numTotalRuns",
        format!("{}", e1.len() + e2.len() + e4.len() + e7.len()),
    ));
    numbers.push_str(&m("numEoneRuns", format!("{}", e1.len())));
    numbers.push_str(&m("partLongestSec", format!("{:.0}", longest as f64 * 0.1)));
    numbers.push_str(&m("sfEdgeSlope", format!("{sf_slope:.3}")));
    numbers.push_str(&m("elEdgeSlope", format!("{el_slope:.3}")));
    numbers.push_str(&m("tostDiff", format!("{tost_diff:.3}")));
    numbers.push_str(&m("tostP", format!("{tost_p:.3}")));
    numbers.push_str(&m("logrankChi", format!("{lr_chi:.1}")));
    numbers.push_str(&m("logrankP", fmt_p(lr_p)));
    numbers.push_str(&m("pprMinusArgmax", format!("{:.1}", pct(c5_diff))));
    numbers.push_str(&m("pprArgmaxP", fmt_p(c5_p)));
    numbers.push_str(&m("bytesRatioEdge", format!("{ratio_el:.1}")));
    numbers.push_str(&m("bytesRatioHybrid", format!("{ratio_hy:.0}")));
    fs::write(format!("{out}/numbers.tex"), &numbers)?;

    println!(
        "Analysis written to {out}/ (summary_e1..e7.csv, ecdf_e1.csv, stats.txt, numbers.tex, \
         figdata/, fig_sovloc_results.svg)."
    );
    println!("\n{stats}");
    Ok(())
}

fn fmt_opt(x: f64) -> String {
    if x.is_nan() {
        "censored".to_string()
    } else {
        format!("{x:.1}")
    }
}

fn fmt_p(p: f64) -> String {
    // Math-mode safe (used inside $...$ in the manuscript).
    if p < 0.001 {
        "{<}0.001".to_string()
    } else {
        format!("{p:.3}")
    }
}

#[allow(dead_code)]
fn p95_of(xs: &[f64]) -> f64 {
    quantile(xs, 0.95)
}

/// Build the two-panel results figure as a dependency-free SVG. Each panel plots
/// median time to diagnosis (log y, steps) against partition length (linear x,
/// seconds) for the four placements. A censored median (not reached) is drawn as
/// a hollow marker at the top of the axis rather than silently dropped.
fn results_svg(
    placements: &[&str],
    parts: &[usize],
    edge: &BTreeMap<(String, usize), f64>,
    cloud: &BTreeMap<(String, usize), f64>,
) -> String {
    // colour and label per placement
    let style = |pl: &str| -> (&'static str, &'static str) {
        match pl {
            "central-naive" => ("#8c8c8c", "central naive"),
            "central-sf" => ("#d62728", "central store-and-forward"),
            "edge-local" => ("#1f77b4", "edge local"),
            "hybrid" => ("#2ca02c", "hybrid"),
            _ => ("#000000", "other"),
        }
    };
    let max_sec = (*parts.last().unwrap_or(&0) as f64) * 0.1;
    let ymax = 10_000.0_f64; // log decades 1..10000
    let log_ymax = ymax.log10();

    // panel geometry
    let (pw, ph) = (300.0_f64, 300.0_f64); // plot area
    let (ml, mt) = (58.0_f64, 34.0_f64); // left/top margin inside a panel
    let gap = 60.0_f64; // gap between the two panels
    let panel_w = ml + pw + 24.0;
    let total_w = panel_w * 2.0 + gap;
    let total_h = mt + ph + 120.0; // room for x labels + legend

    let x_px = |x0: f64, sec: f64| x0 + ml + (sec / max_sec.max(1.0)) * pw;
    let y_px = |steps: f64| {
        let s = steps.max(1.0).min(ymax);
        mt + ph - (s.log10() / log_ymax) * ph
    };

    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{total_w:.0}\" height=\"{total_h:.0}\" \
         viewBox=\"0 0 {total_w:.0} {total_h:.0}\" font-family=\"Helvetica,Arial,sans-serif\">\n"
    ));
    s.push_str(&format!(
        "<rect width=\"{total_w:.0}\" height=\"{total_h:.0}\" fill=\"white\"/>\n"
    ));

    for (panel, (data, title)) in [(edge, "Edge-side fault"), (cloud, "Cloud-side fault")]
        .iter()
        .enumerate()
    {
        let x0 = panel as f64 * (panel_w + gap);
        // panel title
        s.push_str(&format!(
            "<text x=\"{:.0}\" y=\"{:.0}\" text-anchor=\"middle\" font-size=\"15\">{}</text>\n",
            x0 + ml + pw / 2.0,
            mt - 14.0,
            title
        ));
        // y grid + labels (log decades)
        for d in 0..=4 {
            let steps = 10f64.powi(d);
            let y = y_px(steps);
            s.push_str(&format!(
                "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"#e6e6e6\"/>\n",
                x0 + ml,
                y,
                x0 + ml + pw,
                y
            ));
            s.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" font-size=\"11\">{}</text>\n",
                x0 + ml - 6.0,
                y + 3.0,
                steps as i64
            ));
        }
        // x axis ticks (proportional, seconds)
        let xticks = [0.0, max_sec * 0.25, max_sec * 0.5, max_sec * 0.75, max_sec];
        for t in xticks {
            let x = x_px(x0, t);
            s.push_str(&format!(
                "<line x1=\"{x:.1}\" y1=\"{:.1}\" x2=\"{x:.1}\" y2=\"{:.1}\" stroke=\"#cccccc\"/>\n",
                mt + ph,
                mt + ph + 4.0
            ));
            s.push_str(&format!(
                "<text x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"11\">{:.0}</text>\n",
                mt + ph + 18.0,
                t
            ));
        }
        // axis box
        s.push_str(&format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"none\" stroke=\"#333\"/>\n",
            x0 + ml, mt, pw, ph
        ));
        // axis titles
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"12\">Partition length (s)</text>\n",
            x0 + ml + pw / 2.0,
            mt + ph + 36.0
        ));
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"12\" transform=\"rotate(-90 {:.1} {:.1})\">Median time to diagnosis (steps)</text>\n",
            x0 + 14.0,
            mt + ph / 2.0,
            x0 + 14.0,
            mt + ph / 2.0
        ));

        // one polyline + markers per placement
        for pl in placements {
            let (colour, _) = style(pl);
            let mut pts: Vec<(f64, f64, bool)> = Vec::new(); // x, y, censored
            for &pt in parts {
                let sec = pt as f64 * 0.1;
                let v = data.get(&(pl.to_string(), pt)).copied().unwrap_or(f64::NAN);
                let censored = v.is_nan();
                let steps = if censored { ymax } else { v };
                pts.push((x_px(x0, sec), y_px(steps), censored));
            }
            let path: String = pts
                .iter()
                .enumerate()
                .map(|(i, (x, y, _))| {
                    format!("{}{:.1},{:.1}", if i == 0 { "M" } else { "L" }, x, y)
                })
                .collect::<Vec<_>>()
                .join(" ");
            s.push_str(&format!(
                "<path d=\"{path}\" fill=\"none\" stroke=\"{colour}\" stroke-width=\"1.8\"/>\n"
            ));
            for (x, y, censored) in &pts {
                if *censored {
                    s.push_str(&format!(
                        "<circle cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"3.4\" fill=\"white\" stroke=\"{colour}\" stroke-width=\"1.6\"/>\n"
                    ));
                } else {
                    s.push_str(&format!(
                        "<circle cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"3\" fill=\"{colour}\"/>\n"
                    ));
                }
            }
        }
    }

    // legend across the bottom
    let ly = mt + ph + 64.0;
    let mut lx = ml;
    for pl in placements {
        let (colour, label) = style(pl);
        s.push_str(&format!(
            "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"4\" fill=\"{colour}\"/>\n",
            lx,
            ly - 4.0
        ));
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"12\">{}</text>\n",
            lx + 10.0,
            ly,
            label
        ));
        lx += 190.0;
        if lx > total_w - 150.0 {
            lx = ml;
        }
    }
    s.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" fill=\"#555\">Hollow marker: median not reached within the run (censored).</text>\n",
        ml,
        ly + 22.0
    ));
    s.push_str("</svg>\n");
    s
}
