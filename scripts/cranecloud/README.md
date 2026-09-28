# Real-network tier: SovLoc across the Internet on Crane Cloud

This runbook deploys the SovLoc **cloud collector** to Crane Cloud and keeps the
**edge** side (the seven services and the edge collector) on your local machine,
so telemetry crosses the real Internet instead of a Docker bridge on one host.

```
local machine (7 edge services + edge collector)  <-- real Internet -->  Crane Cloud (cloud collector)
```

Why this direction: the services and `inject` only dial outbound, and a collector
only accepts inbound, so the collector belongs on the platform that provides a
public inbound endpoint. This is also the semantically correct split, since the
cloud collector is the cloud side.

## What changed in the code (and what did not)

Additive only. The `mesh` binary gained an optional HTTP transport for this tier,
behind new flags, with **no new dependency**:

- collector `--http-port <p>`: serves `POST /telemetry`, `POST /control`, `GET /healthz`.
- service `--http-cloud <url>`: also POSTs each sample to `<url>/telemetry`.
- inject `--http-collectors <url>`: POSTs control to `<url>/control`.

The raw-TCP telemetry and control paths, the simulator, the RCA algorithm, and
the existing Docker/`docker-compose` tiers are unchanged. Crane Cloud exposes an
HTTP(S) ingress and a single port per app, not a raw TCP port, which is why the
telemetry and the inject control are folded onto one HTTP port.

## Prerequisites

- A Docker Hub account and `docker login` done locally.
- A Crane Cloud account (https://cranecloud.io) with a project.
- Docker with Compose v2 locally (for the build).

## Step 1 to 3: build, validate locally, push

```bash
# from the repo root
cargo build --release -p mesh            # sanity build (optional; the image builds too)

DOCKER_USER=<your-dockerhub-username> ./scripts/cranecloud/build_and_push.sh v1
```

`build_and_push.sh` builds the `runtime` image and pushes
`docker.io/<user>/sovloc-mesh:v1`. It prints the exact Crane Cloud settings to use.

## Step 4: deploy on Crane Cloud (console)

Create App, choose Deploy single application, then:

| Field | Value |
| --- | --- |
| Image | `<your-dockerhub-username>/sovloc-mesh:v1` |
| Port | `8080` |
| Entry command | leave blank |
| Replicas | `1` |

The image starts the cloud collector on port 8080 by itself, so the entry command
must be left blank. Do not type a `collector ...` command in that field: Crane
Cloud does not reliably pass a multi-word entry command through to the binary, and
a partial command crash-loops the container into `mesh` with no subcommand (the
symptom is a Failed app and repeated help text in the logs). Optional env vars:
`SOVLOC_STALE_MS` (default 3000), `SOVLOC_NAME` (default cloud), `PORT` (default 8080).

Deploy, wait until the app is Running.

## Step 5: get the public endpoint

App Settings, Domain and URLs, copy the public URL, for example
`https://sovloc-xxxx.cranecloud.io`.

## Step 6 and 7: configure locally and verify connectivity

```bash
./scripts/cranecloud/probe.sh https://sovloc-xxxx.cranecloud.io
```

Expected: `GET /healthz` returns `ok` and `POST /telemetry` returns `204`. That
means the collector is reachable over the real Internet.

Note on http vs https: `--http-cloud` sends plain HTTP because the platform
terminates TLS at its ingress. If the probe shows the endpoint answers on
`http://` (port 80), use the `http://` URL for the run below. If it only answers
on `https://` and refuses `http://`, the local client needs a one-line TLS build.
That is a local rebuild only; the Crane Cloud image does not change. Ask the
maintainer to enable it before the run.

## Step 8 and 9: baseline run, then the real-network run

Before each measured run, restart the Crane Cloud app (App Settings, Disable then
Enable, or Redeploy). The cloud collector is a single long-lived process that
freezes its pre-fault baseline once, so a fresh start is what gives it a clean
baseline over the healthy window before the fault. The local edge collector is a
fresh process each run already, so it needs nothing. If the link is poor and the
cloud shows some services never becoming visible, raise the healthy window with
`BASELINE_WAIT=20` in front of the run command.

Baseline (normal broadband):

```bash
./scripts/cranecloud/run_edge_local.sh http://sovloc-xxxx.cranecloud.io validate 120
```

The seven services and the local edge collector start, freeze a baseline, then a
fault is injected into `validate`. Watch both collectors:

- edge collector output is printed at the end and saved to `results/realnet_edge_*.log`.
- cloud collector output is on the Crane Cloud console under Application logs.

Both should localize `validate` within a second or two, with `visible=7/7`.

Real degraded-network trial (exam section 5.3), on a genuinely uncontrolled link:

```bash
# terminal A: start the link trace on the SAME uplink
./scripts/cranecloud/capture_link_trace.sh https://sovloc-xxxx.cranecloud.io 180

# terminal B: run the mesh, then during the run switch this machine to a weak
# mobile hotspot (or otherwise degrade/cut the uplink to Crane Cloud)
./scripts/cranecloud/run_edge_local.sh http://sovloc-xxxx.cranecloud.io validate 180
```

During the real partition the Crane Cloud cloud collector stops receiving
telemetry and shows `visible=0/7` with no culprit, while the local edge collector
holds `validate` at rank 1 with `visible=7/7`. That is the sovereignty result on
a real Internet path, not a synthesized one.

## Step 10 to 12: collect and preserve raw results

Everything lands under `results/`, captured automatically by the run:

- `results/realnet_edge_*.log`: the local edge collector output (authoritative
  timing, since inject and the edge collector share one clock).
- `results/realnet_cloud_status_*.csv`: the Crane Cloud collector's live view,
  polled once per second over its `GET /status` endpoint (visible count, top-1,
  localization time, and the HTTP code so an unreachable cloud shows as `000`).
  This is the cloud side of the result recorded from your own machine, so it does
  not depend on the platform console.
- `results/realnet_linktrace_*.http.csv`: the real link trace (HTTP round-trip
  time and code to the endpoint over time). ICMP is usually filtered by the
  platform load balancer, so the `.ping.log` may be empty; the HTTP trace is the
  usable one.

Do not edit these. They are the raw record.

## Step 13: update the report

Only after a real run exists, add the measured numbers to the manuscript
Production Validation section and the simulated-vs-production table, in the column
reserved for the real Internet (Crane Cloud) tier. Until then those cells stay
marked TO BE MEASURED. Never fill them with expected values.

## Timing note (honest limitation)

The edge collector and `inject` run on the same machine, so the edge
`LOCALIZATION_TIME_MS` is clean. The Crane Cloud cloud collector runs on a
different clock, so its absolute millisecond value carries the two machines'
clock offset. Keep both machines on NTP and treat the edge value as authoritative
for absolute time; the cloud collector's role is the visibility result under the
real partition, which does not depend on cross-machine time.
