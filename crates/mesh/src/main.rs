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

mod http1;
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
use scenario::Scenario;

/// Build the call graph a mesh process should use: the configured scenario's
/// graph when `--scenario <file>` is given, otherwise the built-in topology. The
/// default keeps every existing command and the committed compose working
/// unchanged.
fn graph_for(path: &Option<String>) -> Result<CallGraph> {
    match path {
        Some(p) => Ok(Scenario::load(p)?.build()?.graph),
        None => Ok(build_graph()),
    }
}

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
    /// Generate a docker-compose file for a scenario (many edge devices, each
    /// with its own collector and partition time), ready for `docker compose up`
    /// or a Crane Cloud deployment.
    GenCompose(GenComposeArgs),
    /// Generate Kubernetes manifests for the full multi-edge mesh (one Deployment
    /// and Service per microservice, one edge collector per device, a cloud
    /// collector, a control pod) plus a per-device cut NetworkPolicy, for the
    /// cluster tiers (k3s / OpenStack / Crane Cloud). A real partition is a
    /// `kubectl apply` of the device's cut policy.
    GenK8s(GenK8sArgs),
    /// Print the scenario's services as CSV (service,device,partition_sec,side),
    /// which the E6 harness uses to pick culprits and their per-device cut length.
    Describe(DescribeArgs),
    /// GET a collector's /status URL and print the JSON body. Lets the E6 cluster
    /// harness read a collector's live view from inside the cluster (kubectl exec)
    /// without a curl/wget in the image.
    Status(StatusArgs),
}

#[derive(Args)]
struct DescribeArgs {
    /// Scenario file (YAML).
    #[arg(long)]
    scenario: String,
}

#[derive(Args)]
struct GenK8sArgs {
    /// Scenario file (YAML).
    #[arg(long)]
    scenario: String,
    /// Output directory for mesh.yaml and the per-device cut-*.yaml policies.
    #[arg(long, default_value = "deploy/k8s/generated")]
    out_dir: String,
    /// Kubernetes namespace.
    #[arg(long, default_value = "sovloc")]
    namespace: String,
    /// Container image for every pod (must carry the mesh binary).
    #[arg(long, default_value = "sovloc-mesh:local")]
    image: String,
    /// imagePullPolicy (e.g. Never for a locally imported image, IfNotPresent for a registry).
    #[arg(long, default_value = "IfNotPresent")]
    image_pull_policy: String,
    /// Directory the scenario ConfigMap is mounted at; the file is <dir>/scenario.yaml.
    #[arg(long, default_value = "/etc/sovloc")]
    mount_dir: String,
    /// Cloud collector telemetry port.
    #[arg(long, default_value_t = 7000)]
    cloud_port: u16,
    /// First edge collector telemetry port; device k uses base + k.
    #[arg(long, default_value_t = 7001)]
    edge_base_port: u16,
    /// Cloud collector HTTP /status port.
    #[arg(long, default_value_t = 8080)]
    cloud_http_port: u16,
    /// First edge collector HTTP /status port; device k uses base + k.
    #[arg(long, default_value_t = 8081)]
    edge_http_base_port: u16,
}

#[derive(Args)]
struct StatusArgs {
    /// Full http:// URL of the collector's status endpoint, e.g.
    /// `http://cloud:8080/status`.
    #[arg(long)]
    url: String,
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
    /// Optional plain HTTP/1.1 ingress port for the wide-area (Crane Cloud) tier,
    /// where the platform exposes an HTTP ingress instead of a raw TCP port. When
    /// set, this serves `POST /telemetry`, `POST /control`, and `GET /healthz` on
    /// the given port, folding both telemetry and inject control onto the single
    /// port the platform can expose. Unset means raw-TCP only (local/Docker tiers).
    #[arg(long)]
    http_port: Option<u16>,
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
    /// Optional scenario file (YAML). When given, the collector builds its call
    /// graph from the scenario instead of the built-in seven-service topology.
    #[arg(long)]
    scenario: Option<String>,
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
    /// Optional wide-area cloud collector as a plain HTTP base URL for the Crane
    /// Cloud tier, e.g. `http://sovloc-xyz.cranecloud.io`. When set, each metric
    /// is also POSTed to `<url>/telemetry`. This is the cloud link in the WAN
    /// experiment; `--edge` (raw TCP) stays the local edge link. A dedicated task
    /// streams each sample over one keep-alive connection via a small bounded
    /// channel, so a blackholed cloud link never stalls the loop or starves the
    /// edge collector, which is exactly the partition behaviour.
    #[arg(long)]
    http_cloud: Option<String>,
    /// Report period in ms.
    #[arg(long, default_value_t = 200)]
    period_ms: u64,
    /// Reopen the CLOUD telemetry connection at least this often (ms). A real
    /// network partition such as a k8s NetworkPolicy only blocks NEW connections,
    /// so an already-established stream would keep the cloud fed and never go
    /// blind; recycling the socket makes the cut take effect within this window
    /// plus the collector staleness window. 0 (the default) keeps one persistent
    /// connection, which is correct wherever the cut severs the socket itself
    /// (the Docker tier's network disconnect); the k8s tier sets this to 1000 via
    /// `gen-k8s` because a NetworkPolicy only blocks new connections. The edge
    /// link is same-device and never recycled.
    #[arg(long, default_value_t = 0)]
    conn_recycle_ms: u64,
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
    /// Optional scenario file (YAML). When given, the service builds its call
    /// graph from the scenario so names beyond the built-in seven resolve.
    #[arg(long)]
    scenario: Option<String>,
}

#[derive(Args)]
struct InjectArgs {
    /// Service to fault, or "none" to clear the fault everywhere.
    #[arg(long)]
    culprit: String,
    /// Service control targets, comma separated host:port list. When omitted it
    /// defaults to the scenario's services (if `--scenario` is given) or the
    /// built-in seven-service list.
    #[arg(long)]
    services: Option<String>,
    /// Collector control targets, comma separated host:port list. When omitted it
    /// defaults to one per edge device plus `cloud` (if `--scenario` is given) or
    /// the built-in `cloud,edge` pair.
    #[arg(long)]
    collectors: Option<String>,
    /// Optional wide-area collector control endpoints as plain HTTP base URLs,
    /// comma separated (Crane Cloud tier), e.g. `http://sovloc-xyz.cranecloud.io`.
    /// Each gets a `POST <url>/control`, so a fault can be injected into a remote
    /// collector through the single port the platform exposes.
    #[arg(long)]
    http_collectors: Option<String>,
    /// Lead time in ms so every target starts the fault at the same instant.
    #[arg(long, default_value_t = 500)]
    lead_ms: u64,
    /// Optional scenario file (YAML). Used to validate the culprit name and to
    /// derive the service and collector target lists when they are not given.
    #[arg(long)]
    scenario: Option<String>,
}

#[derive(Args)]
struct GenComposeArgs {
    /// Scenario file (YAML) describing the edge devices and their services.
    #[arg(long)]
    scenario: String,
    /// Where to write the generated compose file.
    #[arg(long, default_value = "docker-compose.generated.yml")]
    out: String,
    /// Path the scenario file is mounted at inside each container.
    #[arg(long, default_value = "/app/scenario.yaml")]
    mount_path: String,
    /// Cloud collector telemetry port.
    #[arg(long, default_value_t = 7000)]
    cloud_port: u16,
    /// First edge collector telemetry port; device k listens on base + k.
    #[arg(long, default_value_t = 7001)]
    edge_base_port: u16,
    /// Cloud collector HTTP status port, exposed to the host so the E6 harness can
    /// poll GET /status without an in-container HTTP client.
    #[arg(long, default_value_t = 8080)]
    cloud_http_port: u16,
    /// First edge collector HTTP status port; device k exposes base + k.
    #[arg(long, default_value_t = 8081)]
    edge_http_base_port: u16,
    /// Fault-injection time (and partition start), seconds after the shared clock.
    #[arg(long, default_value_t = 15.0)]
    fault_at_sec: f64,
    /// Default culprit baked into the compose (overridable at run time with the
    /// SOVLOC_CULPRIT environment variable). Defaults to the first edge service.
    #[arg(long)]
    culprit: Option<String>,
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
    // Latest snapshot for the read-only GET /status endpoint, so a remote poller
    // can record the collector's live view (visible count, top-1, localization
    // time) into a file without scraping platform console logs. Updated once per
    // RCA tick; never read by the RCA method itself.
    viz: AtomicU64,
    top: Mutex<Option<usize>>,
    loc_ms: AtomicU64,
}

async fn run_collector(a: CollectorArgs) -> Result<()> {
    let graph = graph_for(&a.scenario)?;

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
        viz: AtomicU64::new(0),
        top: Mutex::new(None),
        loc_ms: AtomicU64::new(0),
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

    // Optional wide-area HTTP ingress (Crane Cloud tier). Only spawned when the
    // flag is set; the raw-TCP telemetry and control paths above are unchanged.
    if let Some(hp) = a.http_port {
        spawn_http_ingress(ctx.clone(), hp);
    }

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
                // Publish the live snapshot for GET /status.
                ctx.viz.store(
                    visible.iter().filter(|&&v| v).count() as u64,
                    Ordering::SeqCst,
                );
                *ctx.top.lock().unwrap() = top_service;
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
                        ctx.loc_ms.store(dt as u64, Ordering::SeqCst);
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
                    apply_collector_control(&ctx, &c);
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
        ingest_metric(&ctx, &m);
    }
    Ok(())
}

/// Fold one telemetry sample into the collector state: refresh the service's
/// latest value and last-seen time, and extend the pre-fault baseline until it
/// freezes. Shared by the raw-TCP path (`handle_conn`) and the HTTP path
/// (`spawn_http_ingress`) so both transports measure identically.
fn ingest_metric(ctx: &Ctx, m: &Metric) {
    let Some(idx) = ctx.graph.index_of(&m.service) else {
        return;
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

/// Apply an inject control message to the collector: switch the expected culprit,
/// reset the localization timer to the new fault time, and re-arm the one-shot
/// localization report. Shared by the raw-TCP control path and the HTTP path.
fn apply_collector_control(ctx: &Ctx, c: &CollectorControl) {
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
        Some(i) => println!(
            "[{}] now expecting culprit = {}",
            ctx.name,
            ctx.graph.name(i)
        ),
        None => println!("[{}] fault cleared, back to healthy", ctx.name),
    }
}

/// Build the read-only JSON body for `GET /status` from the latest snapshot, so
/// a remote poller can log the collector's live view over time. Data only; it
/// never feeds back into the RCA method.
fn status_json(ctx: &Ctx) -> String {
    let visible = ctx.viz.load(Ordering::SeqCst);
    let top1 = ctx
        .top
        .lock()
        .unwrap()
        .map(|i| format!("\"{}\"", ctx.graph.name(i)))
        .unwrap_or_else(|| "null".to_string());
    let expect = ctx
        .expect_idx
        .lock()
        .unwrap()
        .map(|i| format!("\"{}\"", ctx.graph.name(i)))
        .unwrap_or_else(|| "null".to_string());
    let localized_ms = if ctx.localized.load(Ordering::SeqCst) {
        ctx.loc_ms.load(Ordering::SeqCst).to_string()
    } else {
        "null".to_string()
    };
    let t_rel = (now_ms().saturating_sub(ctx.start_ms)) as f64 / 1000.0;
    format!(
        "{{\"name\":\"{}\",\"visible\":{},\"n\":{},\"top1\":{},\"expect\":{},\"localized_ms\":{},\"t_rel_s\":{:.1}}}\n",
        ctx.name, visible, ctx.graph.n, top1, expect, localized_ms, t_rel
    )
}

/// Serve the wide-area HTTP ingress: `POST /telemetry` (one `Metric` JSON body),
/// `POST /control` (one `CollectorControl` JSON body), and `GET /healthz`. It
/// reuses the same state helpers as the TCP path, so the localizer behaves the
/// same whether telemetry arrives over raw TCP or over the platform ingress.
fn spawn_http_ingress(ctx: Arc<Ctx>, port: u16) {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[{}] http ingress port {port} unavailable: {e}", ctx.name);
                return;
            }
        };
        println!(
            "[{}] http ingress on 0.0.0.0:{port} (POST /telemetry, POST /control, GET /healthz)",
            ctx.name
        );
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let ctx = ctx.clone();
            tokio::spawn(async move {
                // Keep-alive: serve requests on this connection until the client
                // closes it, so a streaming sender reuses one connection.
                while let Some(req) = http1::read_request(&mut stream).await {
                    match (req.method.as_str(), req.path.as_str()) {
                        // Health endpoints answer ANY method (GET, HEAD, ...) so a
                        // managed platform's readiness/liveness probe passes however
                        // it is phrased; otherwise the ingress marks the app
                        // unhealthy and returns "no available server" (503).
                        (_, "/healthz") | (_, "/") => {
                            http1::write_response(&mut stream, "200 OK", "ok\n").await;
                        }
                        ("GET", "/status") => {
                            http1::write_response(&mut stream, "200 OK", &status_json(&ctx)).await;
                        }
                        ("POST", "/telemetry") => {
                            if let Ok(m) = serde_json::from_slice::<Metric>(&req.body) {
                                ingest_metric(&ctx, &m);
                                http1::write_response(&mut stream, "204 No Content", "").await;
                            } else {
                                http1::write_response(
                                    &mut stream,
                                    "400 Bad Request",
                                    "bad metric\n",
                                )
                                .await;
                            }
                        }
                        ("POST", "/control") => {
                            if let Ok(c) = serde_json::from_slice::<CollectorControl>(&req.body) {
                                apply_collector_control(&ctx, &c);
                                http1::write_response(&mut stream, "200 OK", "ok\n").await;
                            } else {
                                http1::write_response(
                                    &mut stream,
                                    "400 Bad Request",
                                    "bad control\n",
                                )
                                .await;
                            }
                        }
                        _ => {
                            http1::write_response(&mut stream, "404 Not Found", "not found\n").await
                        }
                    }
                }
            });
        }
    });
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
    let graph = Arc::new(graph_for(&a.scenario)?);
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

    let mut edge_conn: Option<TcpStream> = None;
    let mut ticker = tokio::time::interval(Duration::from_millis(a.period_ms));

    // Raw-TCP cloud sender: a dedicated task owns the cloud connection (and the
    // optional recycle), fed by a small bounded channel, so the main loop never
    // blocks on the cloud link. When the cloud is partitioned (a blackhole route or
    // a disconnect) the task's write/reconnect times out and the cloud goes blind,
    // while the loopback edge send in the loop below is never stalled, so the edge
    // collector keeps localizing. Mirrors the HTTP (Crane Cloud) sender above.
    let cloud_raw_tx = a.cloud.as_ref().map(|addr| {
        let addr = addr.clone();
        let recycle = a.conn_recycle_ms as u128;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
        tokio::spawn(async move {
            let mut conn: Option<TcpStream> = None;
            let mut since: u128 = 0;
            while let Some(line) = rx.recv().await {
                let now = now_ms();
                if recycle > 0 && conn.is_some() && now.saturating_sub(since) >= recycle {
                    conn = None;
                }
                if conn.is_none() {
                    match tokio::time::timeout(Duration::from_millis(300), TcpStream::connect(&addr))
                        .await
                    {
                        Ok(Ok(s)) => {
                            conn = Some(s);
                            since = now;
                        }
                        _ => continue, // unreachable while partitioned: shed this sample
                    }
                }
                if let Some(s) = conn.as_mut() {
                    let w = tokio::time::timeout(Duration::from_millis(500), async {
                        s.write_all(line.as_bytes()).await?;
                        s.write_all(b"\n").await
                    })
                    .await;
                    if !matches!(w, Ok(Ok(()))) {
                        conn = None; // write failed or timed out (partitioned)
                    }
                }
            }
        });
        tx
    });

    // Wide-area cloud sender: a dedicated task owns one keep-alive connection and
    // streams telemetry to the Crane Cloud collector, so the main loop never
    // blocks on the WAN link. A small bounded channel sheds samples while the link
    // is down instead of flooding stale telemetry on heal.
    let cloud_tx = a.http_cloud.as_ref().and_then(|base| {
        let mut sender = http1::HttpSender::new(base)?;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                let _ = tokio::time::timeout(
                    Duration::from_millis(1500),
                    sender.post("/telemetry", &line),
                )
                .await;
            }
        });
        Some(tx)
    });
    if a.http_cloud.is_some() && cloud_tx.is_none() {
        eprintln!(
            "[service {}] --http-cloud must be a plain http:// url; cloud telemetry disabled",
            a.name
        );
    }

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

        // Cloud link (raw TCP): hand the sample to the dedicated sender without
        // blocking; drop it if the channel is full (link slow or partitioned),
        // which is exactly the partition behaviour the study measures.
        if let Some(tx) = &cloud_raw_tx {
            if !cloud_dropped {
                let _ = tx.try_send(line.clone());
            }
        }
        // Wide-area cloud link (Crane Cloud): same non-blocking hand-off.
        if let Some(tx) = &cloud_tx {
            if !cloud_dropped {
                let _ = tx.try_send(line.clone());
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
    // Build the topology once, both to validate the culprit name and to derive
    // the target lists when they are not supplied.
    let topo = match &a.scenario {
        Some(p) => Some(Scenario::load(p)?.build()?),
        None => None,
    };
    let graph: CallGraph = match &topo {
        Some(t) => t.graph.clone(),
        None => build_graph(),
    };

    let culprit = if is_none_name(&a.culprit) {
        None
    } else {
        if graph.index_of(&a.culprit).is_none() {
            anyhow::bail!("unknown service: {}", a.culprit);
        }
        Some(a.culprit.clone())
    };
    let at_ms = now_ms() + a.lead_ms as u128;

    // Service control targets: explicit override, else the scenario's services,
    // else the built-in seven-service list.
    let services = match (&a.services, &topo) {
        (Some(s), _) => s.clone(),
        (None, Some(t)) => t
            .graph
            .names
            .iter()
            .map(|n| format!("{n}:{SERVICE_CONTROL_PORT}"))
            .collect::<Vec<_>>()
            .join(","),
        (None, None) => format!(
            "gateway:{p},ingest:{p},validate:{p},device-registry:{p},enrich:{p},store:{p},alerting:{p}",
            p = SERVICE_CONTROL_PORT
        ),
    };
    // Collector control targets: explicit override, else one per edge device
    // plus `cloud`, else the built-in cloud/edge pair.
    let collectors = match (&a.collectors, &topo) {
        (Some(c), _) => c.clone(),
        (None, Some(t)) => {
            let mut v: Vec<String> = t
                .device_ids
                .iter()
                .map(|id| format!("{id}:{COLLECTOR_CONTROL_PORT}"))
                .collect();
            v.push(format!("cloud:{COLLECTOR_CONTROL_PORT}"));
            v.join(",")
        }
        (None, None) => format!("cloud:{COLLECTOR_CONTROL_PORT},edge:{COLLECTOR_CONTROL_PORT}"),
    };

    for target in services.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let msg = serde_json::to_string(&ServiceControl {
            culprit: culprit.clone(),
            at_ms,
        })?;
        send_control(target, &msg).await;
    }
    for target in collectors
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
    // Wide-area collectors (Crane Cloud) reached over HTTP through the one
    // exposed port, folding inject control onto the same ingress as telemetry.
    if let Some(list) = &a.http_collectors {
        let msg = serde_json::to_string(&CollectorControl {
            expect_culprit: culprit.clone(),
            fault_at_ms: at_ms,
        })?;
        for base in list.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match tokio::time::timeout(
                Duration::from_millis(5000),
                http1::post_json(base, "/control", &msg),
            )
            .await
            {
                Ok(Ok(())) => println!("control sent to {base}/control"),
                Ok(Err(e)) => eprintln!("could not reach {base}: {e}"),
                Err(_) => eprintln!("timed out reaching {base}"),
            }
        }
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
        Cmd::GenCompose(a) => run_gen_compose(a),
        Cmd::GenK8s(a) => run_gen_k8s(a),
        Cmd::Describe(a) => run_describe(a),
        Cmd::Status(a) => {
            print!("{}", http1::get_text(&a.url).await?);
            Ok(())
        }
    }
}

/// Print the scenario's services as CSV so the E6 harness can map each service to
/// its edge device, that device's partition length, and its side.
fn run_describe(a: DescribeArgs) -> Result<()> {
    let topo = Scenario::load(&a.scenario)?.build()?;
    println!("service,device,partition_sec,side");
    for i in 0..topo.n() {
        let name = &topo.graph.names[i];
        match topo.service_device[i] {
            Some(d) => println!(
                "{},{},{},edge",
                name, topo.device_ids[d], topo.partition_sec[d]
            ),
            None => println!("{},cloud,0,cloud", name),
        }
    }
    Ok(())
}

// --------------------------------------------------------------- gen-compose --
/// Emit a docker-compose file for a scenario that supports a REAL per-device
/// network partition (not an in-process drop).
///
/// Topology of networks:
///   * `cloud-net`    holds the cloud collector and the cloud microservices.
///   * `<id>-net`     one per edge device, holds that device's edge collector and
///                    its microservices. The cloud collector also joins every
///                    `<id>-net`, so a device reaches the cloud across its own net.
///
/// A partition of device `d` is then a single `docker network disconnect
/// <project>_<id>-net <project>-cloud-1`: the device's services can no longer
/// reach the cloud collector, while their own edge collector (same net) and the
/// cloud microservices (cloud-net) are unaffected. Reconnecting heals it. This is
/// what `scripts/e6/run_events_mesh.sh` drives, and it is a genuine network cut,
/// which is why the demo `--drop-cloud-*` flags are gone.
///
/// Each collector serves GET /status on an exposed HTTP port, so the E6 harness
/// reads its live view (visible count, top-1, localization time) from the host
/// without an in-container HTTP client. A `sovloc-control` helper joins every net
/// so the harness can `docker compose exec sovloc-control mesh inject ...` to set
/// a culprit on any service even while a device is partitioned.
fn run_gen_compose(a: GenComposeArgs) -> Result<()> {
    let scenario = Scenario::load(&a.scenario)?;
    let topo = scenario.build()?;

    // Microservice and device names become docker compose service keys.
    let valid = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    };
    for id in &topo.device_ids {
        if !valid(id) {
            anyhow::bail!(
                "edge-device id '{id}' is not a valid docker compose name (letters, digits, '-', '_', '.')"
            );
        }
    }
    for name in &topo.graph.names {
        if !valid(name) {
            anyhow::bail!(
                "microservice name '{name}' is not a valid docker compose name (letters, digits, '-', '_', '.')"
            );
        }
    }

    let culprit = a
        .culprit
        .clone()
        .unwrap_or_else(|| topo.graph.names[0].clone());
    let mount = &a.mount_path;
    let vol = format!("./{}:{}:ro", a.scenario, mount);
    let fault = a.fault_at_sec;

    // Network names: one per device plus the cloud net. The cloud collector and
    // the control helper join all of them.
    let dev_net = |id: &str| format!("{id}-net");
    let all_nets: Vec<String> = std::iter::once("cloud-net".to_string())
        .chain(topo.device_ids.iter().map(|id| dev_net(id)))
        .collect();
    let all_nets_yaml = format!("[{}]", all_nets.join(", "));

    let mut s = String::new();
    s.push_str(&format!(
        "# Generated by `mesh gen-compose` from {}. Regenerate rather than editing by hand.\n",
        a.scenario
    ));
    s.push_str("# Harness-driven E6 (real per-device network partition):\n");
    s.push_str("#   scripts/e6/run_events_mesh.sh   (brings this up, runs >=30 events)\n");
    s.push_str("# Standalone scheduled demo (one baked fault at t+fault):\n");
    s.push_str(
        "#   SOVLOC_EPOCH=$(( $(date +%s%3N) + 8000 )) SOVLOC_CULPRIT=<svc> docker compose -f <this file> up --build\n",
    );
    s.push_str("# Partition a device by hand (real cut):\n");
    s.push_str(&format!(
        "#   docker network disconnect sovloc-{}_<id>-net sovloc-{}-cloud-1\n",
        scenario.name, scenario.name
    ));
    s.push_str(&format!("name: sovloc-{}\n\n", scenario.name));
    s.push_str("services:\n");

    // Cloud collector: on cloud-net and every device net; HTTP /status exposed.
    s.push_str("  cloud:\n    build: .\n    cap_add: [\"NET_ADMIN\"]\n");
    s.push_str(&format!("    volumes: [\"{vol}\"]\n"));
    s.push_str(&format!(
        "    ports: [\"{p}:{p}\"]\n",
        p = a.cloud_http_port
    ));
    s.push_str(&format!("    networks: {all_nets_yaml}\n"));
    s.push_str(&format!(
        "    command: [\"collector\",\"--name\",\"cloud\",\"--port\",\"{}\",\"--http-port\",\"{}\",\"--scenario\",\"{}\",\"--expect-culprit\",\"${{SOVLOC_CULPRIT:-none}}\",\"--start-epoch-ms\",\"${{SOVLOC_EPOCH:-0}}\",\"--fault-at-sec\",\"{}\",\"--stale-ms\",\"3000\"]\n",
        a.cloud_port, a.cloud_http_port, mount, fault
    ));

    // Control helper: joins every net so inject reaches any service/collector even
    // while a device is partitioned. It runs nothing until the harness execs it.
    s.push_str("  sovloc-control:\n    build: .\n");
    s.push_str(&format!("    volumes: [\"{vol}\"]\n"));
    s.push_str(&format!("    networks: {all_nets_yaml}\n"));
    s.push_str("    entrypoint: [\"sleep\", \"infinity\"]\n");

    // One edge collector per device, on its own net; HTTP /status exposed.
    for (d, id) in topo.device_ids.iter().enumerate() {
        let port = a.edge_base_port as usize + d;
        let hp = a.edge_http_base_port as usize + d;
        s.push_str(&format!(
            "  {id}:\n    build: .\n    cap_add: [\"NET_ADMIN\"]\n"
        ));
        s.push_str(&format!("    volumes: [\"{vol}\"]\n"));
        s.push_str(&format!("    ports: [\"{hp}:{hp}\"]\n"));
        s.push_str(&format!("    networks: [{}]\n", dev_net(id)));
        s.push_str(&format!(
            "    command: [\"collector\",\"--name\",\"{}\",\"--port\",\"{}\",\"--http-port\",\"{}\",\"--scenario\",\"{}\",\"--expect-culprit\",\"${{SOVLOC_CULPRIT:-none}}\",\"--start-epoch-ms\",\"${{SOVLOC_EPOCH:-0}}\",\"--fault-at-sec\",\"{}\",\"--stale-ms\",\"3000\"]\n",
            id, port, hp, mount, fault
        ));
    }

    // One container per microservice. A device's services stream to their edge
    // collector (same net, never cut) and to the cloud collector (across the
    // device net, cut when that net is disconnected from the cloud). Cloud
    // services stream to the cloud collector only.
    for i in 0..topo.n() {
        let name = &topo.graph.names[i];
        s.push_str(&format!(
            "  {name}:\n    build: .\n    cap_add: [\"NET_ADMIN\"]\n"
        ));
        s.push_str(&format!("    volumes: [\"{vol}\"]\n"));
        match topo.service_device[i] {
            Some(d) => {
                let port = a.edge_base_port as usize + d;
                let edge_name = &topo.device_ids[d];
                s.push_str(&format!("    networks: [{}]\n", dev_net(edge_name)));
                s.push_str(&format!(
                    "    command: [\"service\",\"--name\",\"{}\",\"--scenario\",\"{}\",\"--culprit\",\"${{SOVLOC_CULPRIT:-none}}\",\"--start-epoch-ms\",\"${{SOVLOC_EPOCH:-0}}\",\"--fault-at-sec\",\"{}\",\"--cloud\",\"cloud:{}\",\"--edge\",\"{}:{}\"]\n",
                    name, mount, fault, a.cloud_port, edge_name, port
                ));
            }
            None => {
                s.push_str("    networks: [cloud-net]\n");
                s.push_str(&format!(
                    "    command: [\"service\",\"--name\",\"{}\",\"--scenario\",\"{}\",\"--culprit\",\"${{SOVLOC_CULPRIT:-none}}\",\"--start-epoch-ms\",\"${{SOVLOC_EPOCH:-0}}\",\"--fault-at-sec\",\"{}\",\"--cloud\",\"cloud:{}\"]\n",
                    name, mount, fault, a.cloud_port
                ));
            }
        }
    }

    // Network definitions.
    s.push_str("\nnetworks:\n");
    for net in &all_nets {
        s.push_str(&format!("  {net}: {{}}\n"));
    }

    std::fs::write(&a.out, &s)?;
    let containers = topo.n() + topo.device_ids.len() + 2; // services + edge collectors + cloud + control
    println!(
        "Wrote {} for scenario '{}': {} edge devices, {} microservices, {} containers.",
        a.out,
        scenario.name,
        topo.device_ids.len(),
        topo.n(),
        containers
    );
    println!("Run the E6 production harness (real per-device cut, >=30 events):");
    println!(
        "  COMPOSE_FILE={} PLATFORM=single-node-mesh EVENTS=30 SCENARIO={} scripts/e6/run_events_mesh.sh",
        a.out, a.scenario
    );
    println!(
        "Or a standalone scheduled demo: SOVLOC_EPOCH=$(( $(date +%s%3N) + 8000 )) SOVLOC_CULPRIT={} docker compose -f {} up --build",
        culprit, a.out
    );
    Ok(())
}

// ----------------------------------------------------------------- gen-k8s ---
/// Emit Kubernetes manifests for the full multi-edge mesh and a per-device cut
/// NetworkPolicy. The pod labels (`device=<id>`) let a single policy restrict a
/// device's egress to its own device pods plus DNS, which blocks the cloud
/// collector while leaving the device's own edge collector reachable: a real
/// network partition applied with `kubectl apply -f cut-<id>.yaml` and healed
/// with `kubectl delete -f cut-<id>.yaml`. k3s enforces NetworkPolicy by default.
fn run_gen_k8s(a: GenK8sArgs) -> Result<()> {
    let scenario = Scenario::load(&a.scenario)?;
    let topo = scenario.build()?;

    let valid = |s: &str| {
        !s.is_empty()
            && s.len() <= 63
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    };
    for id in &topo.device_ids {
        if !valid(id) {
            anyhow::bail!("edge-device id '{id}' is not a valid Kubernetes name");
        }
    }
    for name in &topo.graph.names {
        if !valid(name) {
            anyhow::bail!("microservice name '{name}' is not a valid Kubernetes name");
        }
    }

    let ns = &a.namespace;
    let img = &a.image;
    let pull = &a.image_pull_policy;
    let scen_path = format!("{}/scenario.yaml", a.mount_dir);
    let scen_text = std::fs::read_to_string(&a.scenario)?;

    // A Deployment whose single container runs the mesh with the given args (or a
    // sleep for the control pod), with the scenario ConfigMap mounted.
    let deploy = |name: &str,
                  role: &str,
                  device: Option<&str>,
                  args: &[String],
                  sleep: bool|
     -> String {
        let dev_label = device
            .map(|d| format!("\n    device: {d}"))
            .unwrap_or_default();
        let dev_label_t = device
            .map(|d| format!("\n        device: {d}"))
            .unwrap_or_default();
        let cmd = if sleep {
            "        command: [\"sleep\", \"infinity\"]\n".to_string()
        } else {
            let a: Vec<String> = args.iter().map(|x| format!("\"{x}\"")).collect();
            format!("        args: [{}]\n", a.join(", "))
        };
        format!(
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {name}\n  namespace: {ns}\n  labels:\n    app: {name}\n    sovloc-role: {role}{dev_label}\nspec:\n  replicas: 1\n  selector:\n    matchLabels:\n      app: {name}\n  template:\n    metadata:\n      labels:\n        app: {name}\n        sovloc-role: {role}{dev_label_t}\n    spec:\n      containers:\n      - name: mesh\n        image: {img}\n        imagePullPolicy: {pull}\n{cmd}        volumeMounts:\n        - name: scenario\n          mountPath: {mount}\n          readOnly: true\n      volumes:\n      - name: scenario\n        configMap:\n          name: sovloc-scenario\n---\n",
            mount = a.mount_dir
        )
    };
    let service = |name: &str, ports: &[(&str, u16)]| -> String {
        let mut p = String::new();
        for (pn, port) in ports {
            p.push_str(&format!(
                "  - {{ name: {pn}, port: {port}, targetPort: {port} }}\n"
            ));
        }
        format!(
            "apiVersion: v1\nkind: Service\nmetadata:\n  name: {name}\n  namespace: {ns}\nspec:\n  selector:\n    app: {name}\n  ports:\n{p}---\n"
        )
    };

    let mut m = String::new();
    m.push_str(&format!(
        "# Generated by `mesh gen-k8s` from {}. Regenerate rather than editing by hand.\n",
        a.scenario
    ));
    m.push_str(
        "# Apply:   kubectl apply -f mesh.yaml\n# Partition a device (real cut): kubectl apply -f cuts/cut-<id>.yaml ; heal: kubectl delete -f cuts/cut-<id>.yaml\n",
    );
    // Namespace.
    m.push_str(&format!(
        "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: {ns}\n---\n"
    ));
    // Scenario ConfigMap (indented block scalar).
    let mut indented = String::new();
    for l in scen_text.lines() {
        indented.push_str("    ");
        indented.push_str(l);
        indented.push('\n');
    }
    m.push_str(&format!(
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: sovloc-scenario\n  namespace: {ns}\ndata:\n  scenario.yaml: |\n{indented}---\n"
    ));
    // Control pod.
    m.push_str(&deploy("sovloc-control", "control", None, &[], true));

    // Cloud collector + Service.
    let cloud_args: Vec<String> = [
        "collector",
        "--name",
        "cloud",
        "--port",
        &a.cloud_port.to_string(),
        "--http-port",
        &a.cloud_http_port.to_string(),
        "--scenario",
        &scen_path,
        "--stale-ms",
        "3000",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    m.push_str(&deploy("cloud", "cloud", None, &cloud_args, false));
    m.push_str(&service(
        "cloud",
        &[
            ("telemetry", a.cloud_port),
            ("control", 9101),
            ("http", a.cloud_http_port),
        ],
    ));

    // Edge collectors + Services.
    for (d, id) in topo.device_ids.iter().enumerate() {
        let tport = a.edge_base_port + d as u16;
        let hport = a.edge_http_base_port + d as u16;
        let args: Vec<String> = [
            "collector",
            "--name",
            id,
            "--port",
            &tport.to_string(),
            "--http-port",
            &hport.to_string(),
            "--scenario",
            &scen_path,
            "--stale-ms",
            "3000",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        m.push_str(&deploy(id, "edge-collector", Some(id), &args, false));
        m.push_str(&service(
            id,
            &[("telemetry", tport), ("control", 9101), ("http", hport)],
        ));
    }

    // One Deployment + Service per microservice.
    for i in 0..topo.n() {
        let name = &topo.graph.names[i];
        let mut args: Vec<String> = vec![
            "service".into(),
            "--name".into(),
            name.clone(),
            "--scenario".into(),
            scen_path.clone(),
            "--culprit".into(),
            "none".into(),
            "--cloud".into(),
            format!("cloud:{}", a.cloud_port),
            // A k8s NetworkPolicy partition only blocks NEW connections, so the
            // service must recycle its cloud socket for the cut to take effect.
            // The Docker tier severs the socket directly (network disconnect) and
            // leaves this at the default 0, avoiding reconnect churn on one host.
            "--conn-recycle-ms".into(),
            "1000".into(),
        ];
        match topo.service_device[i] {
            Some(d) => {
                let tport = a.edge_base_port + d as u16;
                let id = &topo.device_ids[d];
                args.push("--edge".into());
                args.push(format!("{id}:{tport}"));
                m.push_str(&deploy(name, "edge-svc", Some(id), &args, false));
            }
            None => {
                m.push_str(&deploy(name, "cloud-svc", None, &args, false));
            }
        }
        m.push_str(&service(name, &[("control", 9100)]));
    }

    // Write the manifest and the per-device cut policies.
    std::fs::create_dir_all(&a.out_dir)?;
    std::fs::create_dir_all(format!("{}/cuts", a.out_dir))?;
    std::fs::write(format!("{}/mesh.yaml", a.out_dir), &m)?;
    for id in &topo.device_ids {
        let np = format!(
            "# Real partition of device {id}: restrict its egress to its own device\n# pods plus DNS, which blocks the cloud collector. apply=cut, delete=heal.\napiVersion: networking.k8s.io/v1\nkind: NetworkPolicy\nmetadata:\n  name: cut-{id}\n  namespace: {ns}\nspec:\n  podSelector:\n    matchLabels:\n      device: {id}\n  policyTypes: [Egress]\n  egress:\n  - to:\n    - podSelector:\n        matchLabels:\n          device: {id}\n  - to:\n    - namespaceSelector: {{}}\n      podSelector:\n        matchLabels:\n          k8s-app: kube-dns\n    ports:\n    - {{ protocol: UDP, port: 53 }}\n    - {{ protocol: TCP, port: 53 }}\n"
        );
        std::fs::write(format!("{}/cuts/cut-{id}.yaml", a.out_dir), np)?;
    }

    let objs = (topo.n() + topo.device_ids.len() + 1) * 2 + 3; // deploy+svc each (-control svc) + ns+cm+control
    println!(
        "Wrote {}/mesh.yaml and {} cut policies for '{}': {} edge devices, {} microservices (~{} objects).",
        a.out_dir,
        topo.device_ids.len(),
        scenario.name,
        topo.device_ids.len(),
        topo.n(),
        objs
    );
    println!("Deploy:  kubectl apply -f {}/mesh.yaml", a.out_dir);
    println!(
        "Run E6:  SCENARIO={} NAMESPACE={} PLATFORM=k3s EVENTS=30 scripts/e6/run_events_k8s.sh",
        a.scenario, ns
    );
    Ok(())
}
