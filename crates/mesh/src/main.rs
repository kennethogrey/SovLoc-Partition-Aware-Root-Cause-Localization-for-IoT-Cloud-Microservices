//! SovLoc production-tier mesh (real network, real partition).
//!
//! Two roles, launched as separate OS processes (or Docker containers):
//!
//!   * `collector`, a live RCA analyzer. It receives streamed telemetry over
//!     TCP, keeps a per-service baseline, and every 500 ms runs the SAME
//!     `rca-core` localizer over the services it can currently see fresh. Run
//!     two of them: a `cloud` collector (reachable across the edge to cloud
//!     link) and an `edge` collector (co-located with the edge services). A real
//!     partition (`tc/netem` or `docker network disconnect`) stops edge telemetry
//!     reaching the cloud collector, so its view goes stale and it can no longer
//!     localize an edge fault, while the edge collector keeps localizing, the
//!     operational-sovereignty result, on real infrastructure.
//!
//!   * `service`, one microservice. Every 200 ms it reports its current metric
//!     (computed from the shared model, so a fault at the current culprit
//!     propagates realistically up the call graph) to both collectors.
//!
//! A third role, `inject`, sends a control message to the running services and
//! collectors so you can pick any service, inject a fault into it at runtime, and
//! watch the localizer move its top-1 to that service live. Run `mesh inject
//! --culprit store` while the mesh is up, or `--culprit none` to clear.
//!
//! All processes share a clock zero via `--start-epoch-ms`, so a collector can
//! print `LOCALIZATION_TIME_MS <n>` automatically. If no epoch is given the
//! services start healthy and wait for an injection, rather than mis-scheduling a
//! fault. See the README and `scripts/` for launch and partition helpers.

mod model;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use rand::{Rng, SeedableRng};
use rca_core::stats::robust_score;
use rca_core::{localize, rank_of, CallGraph, RcaConfig};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use model::{ancestor_distances, build_graph, service_value};

/// Consecutive correct top-1 results a collector needs before it declares the
/// culprit localized. This mirrors the confirmation window in the simulator so
/// the two tiers measure localization the same way, and it stops a chance
/// pre-fault ranking from reporting a meaningless localization time.
const LOCALIZE_STABILITY: usize = 3;

/// Default control ports. Services listen on one, collectors on the other, so a
/// single `inject` command can reach both over the compose network.
const SERVICE_CONTROL_PORT: u16 = 9100;
const COLLECTOR_CONTROL_PORT: u16 = 9101;

#[derive(Serialize, Deserialize)]
struct Metric {
    service: String,
    value: f64,
    ts_ms: u128,
}

/// Control message sent to a `service`: set the current culprit (or `None` to
/// clear it) and the wall-clock time at which the fault starts.
#[derive(Serialize, Deserialize)]
struct ServiceControl {
    culprit: Option<String>,
    at_ms: u128,
}

/// Control message sent to a `collector`: set the service it should now expect as
/// the culprit (for the status line and the localization timer), and the fault
/// time to measure localization from.
#[derive(Serialize, Deserialize)]
struct CollectorControl {
    expect_culprit: Option<String>,
    fault_at_ms: u128,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

/// True when the name is empty or the literal "none", meaning "no culprit".
fn is_none_name(name: &str) -> bool {
    name.is_empty() || name.eq_ignore_ascii_case("none")
}

#[derive(Parser)]
#[command(about = "SovLoc production-tier mesh: real services + two live RCA collectors")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a live RCA collector (run one as `cloud`, one as `edge`).
    Collector(CollectorArgs),
    /// Run one microservice that streams telemetry to the collectors.
    Service(ServiceArgs),
    /// Inject (or clear) a fault in the running mesh and tell the collectors.
    Inject(InjectArgs),
}

#[derive(Args)]
struct CollectorArgs {
    /// Label for logs (e.g., "cloud" or "edge").
    #[arg(long)]
    name: String,
    /// TCP port to listen on for telemetry.
    #[arg(long)]
    port: u16,
    /// Port to listen on for control messages from `inject`.
    #[arg(long, default_value_t = COLLECTOR_CONTROL_PORT)]
    control_port: u16,
    /// The known injected culprit (for automatic localization-time reporting).
    /// "none" or empty means start healthy and wait for an injection.
    #[arg(long)]
    expect_culprit: Option<String>,
    /// Shared clock zero (unix ms) passed to every process. 0 means no schedule.
    #[arg(long, default_value_t = 0)]
    start_epoch_ms: u128,
    /// When the fault is injected, seconds after the shared clock zero.
    #[arg(long, default_value_t = 15.0)]
    fault_at_sec: f64,
    /// A service is "visible" only if seen within this many ms.
    #[arg(long, default_value_t = 1500)]
    stale_ms: u64,
    /// Samples used to freeze the pre-fault baseline.
    #[arg(long, default_value_t = 40)]
    baseline_n: usize,
}

#[derive(Args)]
struct ServiceArgs {
    /// This service's name (must be one of the seven canonical names).
    #[arg(long)]
    name: String,
    /// The faulted service at start. "none" or empty starts healthy.
    #[arg(long)]
    culprit: String,
    /// Port to listen on for control messages from `inject`.
    #[arg(long, default_value_t = SERVICE_CONTROL_PORT)]
    control_port: u16,
    /// Shared clock zero (unix ms). 0 means no schedule, start healthy.
    #[arg(long, default_value_t = 0)]
    start_epoch_ms: u128,
    /// Fault injection time, seconds after clock zero.
    #[arg(long, default_value_t = 15.0)]
    fault_at_sec: f64,
    /// Cloud collector address (host:port).
    #[arg(long)]
    cloud: Option<String>,
    /// Edge collector address (host:port).
    #[arg(long)]
    edge: Option<String>,
    /// Report period in ms.
    #[arg(long, default_value_t = 200)]
    period_ms: u64,
    /// RNG seed.
    #[arg(long, default_value_t = 7)]
    seed: u64,
    /// DEMO-ONLY partition emulation for machines without `tc/netem`: seconds
    /// after clock zero at which this service stops sending to the CLOUD
    /// collector (0 with dur 0 = disabled). The genuine degraded-network trial
    /// uses real `tc/netem` and does not need this flag.
    #[arg(long, default_value_t = 0.0)]
    drop_cloud_after_sec: f64,
    /// Duration of the demo cloud-link drop, in seconds (0 = disabled).
    #[arg(long, default_value_t = 0.0)]
    drop_cloud_dur_sec: f64,
}

#[derive(Args)]
struct InjectArgs {
    /// Service to fault, or "none" to clear the fault everywhere.
    #[arg(long)]
    culprit: String,
    /// Service control targets, comma separated host:port list.
    #[arg(
        long,
        default_value = "gateway:9100,ingest:9100,validate:9100,device-registry:9100,enrich:9100,store:9100,alerting:9100"
    )]
    services: String,
    /// Collector control targets, comma separated host:port list.
    #[arg(long, default_value = "cloud:9101,edge:9101")]
    collectors: String,
    /// Lead time in ms so every target starts the fault at the same instant.
    #[arg(long, default_value_t = 500)]
    lead_ms: u64,
}

// ------------------------------------------------------------------- fault ---
/// The current fault the services report against. `culprit_idx = None` is healthy
/// (every service reports its clean baseline plus noise).
struct FaultState {
    culprit_idx: Option<usize>,
    fault_t0_ms: u128,
    dist: Vec<Option<usize>>,
}

impl FaultState {
    fn healthy(n: usize) -> Self {
        FaultState {
            culprit_idx: None,
            fault_t0_ms: 0,
            dist: vec![None; n],
        }
    }
}

// ---------------------------------------------------------------- collector --
struct ServiceState {
    baseline: Vec<f64>,
    frozen: bool,
    latest: f64,
    last_seen: u128,
}

struct Ctx {
    name: String,
    graph: CallGraph,
    state: Mutex<HashMap<usize, ServiceState>>,
    stale_ms: u64,
    baseline_n: usize,
    expect_idx: Mutex<Option<usize>>,
    fault_t0_ms: AtomicU64,
    localized: AtomicBool,
    start_ms: u128,
}

async fn run_collector(a: CollectorArgs) -> Result<()> {
    let graph = build_graph();

    // Decide the starting expectation. A named culprit with a real shared clock
    // schedules the fault; "none", empty, or a missing epoch starts healthy and
    // waits for an injection, which also stops a missing epoch from poisoning the
    // frozen baseline.
    let named = a
        .expect_culprit
        .as_deref()
        .filter(|c| !is_none_name(c))
        .and_then(|c| graph.index_of(c));
    let scheduled = a.start_epoch_ms != 0 && named.is_some();
    let init_expect = if scheduled { named } else { None };
    let init_t0 = if scheduled {
        a.start_epoch_ms + (a.fault_at_sec * 1000.0) as u128
    } else {
        0
    };

    let ctx = Arc::new(Ctx {
        name: a.name.clone(),
        graph,
        state: Mutex::new(HashMap::new()),
        stale_ms: a.stale_ms,
        baseline_n: a.baseline_n,
        expect_idx: Mutex::new(init_expect),
        fault_t0_ms: AtomicU64::new(init_t0 as u64),
        localized: AtomicBool::new(false),
        start_ms: now_ms(),
    });

    match init_expect {
        Some(i) => println!(
            "[{}] scheduled culprit = {} at t+{:.0}s",
            a.name,
            ctx.graph.name(i),
            a.fault_at_sec
        ),
        None => println!("[{}] healthy, waiting for an injection", a.name),
    }

    spawn_collector_control(ctx.clone(), a.control_port);

    // Periodic RCA loop.
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(500));
            let mut last_top: Option<usize> = None;
            let mut tick: u64 = 0;
            let mut correct_run: usize = 0;
            let mut run_start_ms: u128 = 0;
            loop {
                ticker.tick().await;
                tick += 1;
                let now = now_ms();
                let n = ctx.graph.n;
                let mut anomaly = vec![0.0; n];
                let mut visible = vec![false; n];
                {
                    let st = ctx.state.lock().unwrap();
                    for (idx, s) in st.iter() {
                        let fresh = now.saturating_sub(s.last_seen) < ctx.stale_ms as u128;
                        if s.frozen && fresh {
                            visible[*idx] = true;
                            anomaly[*idx] = robust_score(&s.baseline, s.latest);
                        }
                    }
                }
                let ranked = localize(&ctx.graph, &anomaly, &visible, &RcaConfig::default());
                let top_service = ranked.first().map(|r| r.service);
                if let Some(top) = ranked.first() {
                    if last_top != Some(top.service) {
                        last_top = Some(top.service);
                        println!(
                            "[{}] top-1 = {} (score {:.4})",
                            ctx.name,
                            ctx.graph.name(top.service),
                            top.score
                        );
                    }
                }

                // Localization time, gated the same way the simulator gates it:
                // only after the fault time, and only once the expected culprit
                // has held top-1 for a short confirmation run.
                let exp = *ctx.expect_idx.lock().unwrap();
                let t0 = ctx.fault_t0_ms.load(Ordering::SeqCst) as u128;
                if let Some(exp) = exp {
                    if top_service == Some(exp) && now >= t0 {
                        if correct_run == 0 {
                            run_start_ms = now;
                        }
                        correct_run += 1;
                    } else {
                        correct_run = 0;
                    }
                    if correct_run >= LOCALIZE_STABILITY
                        && !ctx.localized.swap(true, Ordering::SeqCst)
                    {
                        let dt = run_start_ms.saturating_sub(t0);
                        println!(
                            "[{}] LOCALIZATION_TIME_MS {}  (culprit={})",
                            ctx.name,
                            dt,
                            ctx.graph.name(exp)
                        );
                    }
                } else {
                    correct_run = 0;
                }

                // Heartbeat every ~2s. This makes the partition legible in a live
                // demo: the edge collector keeps printing "rank 1" for the culprit,
                // while the cloud collector prints "not visible" for the whole
                // partition window because the edge telemetry can no longer reach
                // it. It reports status only; it does not touch the RCA method.
                if tick % 4 == 0 {
                    let n_visible = visible.iter().filter(|&&v| v).count();
                    let top_name = ranked
                        .first()
                        .map(|r| ctx.graph.name(r.service).to_string())
                        .unwrap_or_else(|| "none".to_string());
                    let culprit_status = match exp {
                        Some(e) => match rank_of(&ranked, e) {
                            Some(r) => format!("rank {r}"),
                            None => "not visible".to_string(),
                        },
                        None => "n/a".to_string(),
                    };
                    let culprit_name = exp
                        .map(|e| ctx.graph.name(e).to_string())
                        .unwrap_or_else(|| "none".to_string());
                    let t_rel = (now.saturating_sub(ctx.start_ms)) as f64 / 1000.0;
                    println!(
                        "[{}] t+{:>5.1}s  top-1={:<15} culprit({})={:<12} visible={}/{}",
                        ctx.name, t_rel, top_name, culprit_name, culprit_status, n_visible, n
                    );
                }
            }
        });
    }

    let listener = TcpListener::bind(("0.0.0.0", a.port)).await?;
    println!("[{}] collector listening on 0.0.0.0:{}", a.name, a.port);
    loop {
        let (stream, _) = listener.accept().await?;
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, ctx).await {
                eprintln!("connection ended: {e}");
            }
        });
    }
}

/// Listen for `CollectorControl` lines and update the expected culprit live.
fn spawn_collector_control(ctx: Arc<Ctx>, port: u16) {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[{}] control port {port} unavailable: {e}", ctx.name);
                return;
            }
        };
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stream);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(c) = serde_json::from_str::<CollectorControl>(&line) else {
                        continue;
                    };
                    let new_idx = c
                        .expect_culprit
                        .as_deref()
                        .filter(|s| !is_none_name(s))
                        .and_then(|s| ctx.graph.index_of(s));
                    *ctx.expect_idx.lock().unwrap() = new_idx;
                    ctx.fault_t0_ms
                        .store(c.fault_at_ms as u64, Ordering::SeqCst);
                    ctx.localized.store(false, Ordering::SeqCst);
                    match new_idx {
                        Some(i) => {
                            println!(
                                "[{}] now expecting culprit = {}",
                                ctx.name,
                                ctx.graph.name(i)
                            )
                        }
                        None => println!("[{}] fault cleared, back to healthy", ctx.name),
                    }
                }
            });
        }
    });
}

async fn handle_conn(stream: TcpStream, ctx: Arc<Ctx>) -> Result<()> {
    let reader = BufReader::new(stream);
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let m: Metric = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let Some(idx) = ctx.graph.index_of(&m.service) else {
            continue;
        };
        let mut st = ctx.state.lock().unwrap();
        let entry = st.entry(idx).or_insert_with(|| ServiceState {
            baseline: Vec::new(),
            frozen: false,
            latest: m.value,
            last_seen: m.ts_ms,
        });
        entry.latest = m.value;
        entry.last_seen = m.ts_ms;
        if !entry.frozen {
            entry.baseline.push(m.value);
            if entry.baseline.len() >= ctx.baseline_n {
                entry.frozen = true;
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ service --
async fn send_line(addr: &str, conn: &mut Option<TcpStream>, line: &str) {
    if conn.is_none() {
        match TcpStream::connect(addr).await {
            Ok(s) => *conn = Some(s),
            Err(_) => return, // collector unreachable (e.g., partitioned), best effort
        }
    }
    if let Some(s) = conn.as_mut() {
        if s.write_all(line.as_bytes()).await.is_err() || s.write_all(b"\n").await.is_err() {
            *conn = None;
        }
    }
}

async fn run_service(a: ServiceArgs) -> Result<()> {
    let graph = Arc::new(build_graph());
    let idx = graph
        .index_of(&a.name)
        .ok_or_else(|| anyhow::anyhow!("unknown service name: {}", a.name))?;
    let n = graph.n;

    // Start healthy unless a real shared clock and a named culprit schedule a
    // fault. A missing epoch (0) therefore starts healthy rather than firing a
    // 1970 fault that would poison the collector baseline.
    let named = if is_none_name(&a.culprit) {
        None
    } else {
        Some(
            graph
                .index_of(&a.culprit)
                .ok_or_else(|| anyhow::anyhow!("unknown culprit name: {}", a.culprit))?,
        )
    };
    let scheduled = a.start_epoch_ms != 0 && named.is_some();
    let fault_state = Arc::new(Mutex::new(if scheduled {
        let culprit = named.unwrap();
        FaultState {
            culprit_idx: Some(culprit),
            fault_t0_ms: a.start_epoch_ms + (a.fault_at_sec * 1000.0) as u128,
            dist: ancestor_distances(&graph, culprit),
        }
    } else {
        FaultState::healthy(n)
    }));

    match fault_state.lock().unwrap().culprit_idx {
        Some(c) => println!(
            "[service {}] reporting (scheduled culprit = {})",
            a.name,
            graph.name(c)
        ),
        None => println!(
            "[service {}] reporting (healthy, awaiting injection)",
            a.name
        ),
    }

    spawn_service_control(
        graph.clone(),
        fault_state.clone(),
        a.control_port,
        a.name.clone(),
    );

    // Seed from name + provided seed so each service is deterministic.
    let name_hash: u64 = a
        .name
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    let mut rng = rand::rngs::StdRng::seed_from_u64(a.seed ^ name_hash);

    let mut cloud_conn: Option<TcpStream> = None;
    let mut edge_conn: Option<TcpStream> = None;
    let mut ticker = tokio::time::interval(Duration::from_millis(a.period_ms));

    loop {
        ticker.tick().await;
        let now = now_ms();
        let noise = rng.gen_range(-3.0..3.0);
        let value = {
            let fs = fault_state.lock().unwrap();
            let secs_since_fault = (now as f64 - fs.fault_t0_ms as f64) / 1000.0;
            service_value(idx, &fs.dist, secs_since_fault, noise)
        };
        let m = Metric {
            service: a.name.clone(),
            value,
            ts_ms: now,
        };
        let line = serde_json::to_string(&m)?;

        // DEMO-ONLY: emulate an edge to cloud partition by skipping cloud sends
        // during the configured window (real trials use tc/netem instead).
        let rel_sec = (now as f64 - a.start_epoch_ms as f64) / 1000.0;
        let cloud_dropped = a.drop_cloud_dur_sec > 0.0
            && rel_sec >= a.drop_cloud_after_sec
            && rel_sec < a.drop_cloud_after_sec + a.drop_cloud_dur_sec;
        if cloud_dropped {
            cloud_conn = None; // force reconnect (which will also fail) after heal
        }

        if let Some(addr) = &a.cloud {
            if !cloud_dropped {
                send_line(addr, &mut cloud_conn, &line).await;
            }
        }
        if let Some(addr) = &a.edge {
            send_line(addr, &mut edge_conn, &line).await;
        }
    }
}

/// Listen for `ServiceControl` lines and switch the reported fault live.
fn spawn_service_control(
    graph: Arc<CallGraph>,
    fault_state: Arc<Mutex<FaultState>>,
    port: u16,
    name: String,
) {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[service {name}] control port {port} unavailable: {e}");
                return;
            }
        };
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let graph = graph.clone();
            let fault_state = fault_state.clone();
            let name = name.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stream);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(c) = serde_json::from_str::<ServiceControl>(&line) else {
                        continue;
                    };
                    let culprit_idx = c
                        .culprit
                        .as_deref()
                        .filter(|s| !is_none_name(s))
                        .and_then(|s| graph.index_of(s));
                    let mut fs = fault_state.lock().unwrap();
                    match culprit_idx {
                        Some(i) => {
                            fs.culprit_idx = Some(i);
                            fs.fault_t0_ms = c.at_ms;
                            fs.dist = ancestor_distances(&graph, i);
                            println!("[service {name}] fault set to {}", graph.name(i));
                        }
                        None => {
                            *fs = FaultState::healthy(graph.n);
                            println!("[service {name}] fault cleared");
                        }
                    }
                }
            });
        }
    });
}

// ------------------------------------------------------------------- inject --
async fn send_control(addr: &str, msg: &str) {
    match TcpStream::connect(addr).await {
        Ok(mut s) => {
            let _ = s.write_all(msg.as_bytes()).await;
            let _ = s.write_all(b"\n").await;
        }
        Err(e) => eprintln!("could not reach {addr}: {e}"),
    }
}

async fn run_inject(a: InjectArgs) -> Result<()> {
    let culprit = if is_none_name(&a.culprit) {
        None
    } else {
        // Validate the name against the known topology before sending it out.
        let g = build_graph();
        if g.index_of(&a.culprit).is_none() {
            anyhow::bail!("unknown service: {}", a.culprit);
        }
        Some(a.culprit.clone())
    };
    let at_ms = now_ms() + a.lead_ms as u128;

    for target in a
        .services
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let msg = serde_json::to_string(&ServiceControl {
            culprit: culprit.clone(),
            at_ms,
        })?;
        send_control(target, &msg).await;
    }
    for target in a
        .collectors
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let msg = serde_json::to_string(&CollectorControl {
            expect_culprit: culprit.clone(),
            fault_at_ms: at_ms,
        })?;
        send_control(target, &msg).await;
    }

    match &culprit {
        Some(c) => println!("injected fault at {c} (at_ms={at_ms})"),
        None => println!("cleared the fault everywhere"),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Collector(a) => run_collector(a).await,
        Cmd::Service(a) => run_service(a).await,
        Cmd::Inject(a) => run_inject(a).await,
    }
}
