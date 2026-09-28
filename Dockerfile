# SovLoc is built and run entirely inside Docker, so the only thing a grader
# needs on the host is Docker with Compose v2. There are three stages.
#
#   build      compiles the whole Rust workspace once.
#   reproduce  adds Python and matplotlib and runs the full reproduction
#              (build, unit tests, the seeded experiment, then the statistics
#              and figures). This is what the one command uses.
#   runtime    a small image that ships only the mesh binary plus tc (iproute2)
#              so docker-compose can stand up the real container mesh and a real
#              network partition for the production tier.
#
# Pin by digest for full reproducibility (exam section 4.4): after the first
# pull run
#   docker inspect --format='{{index .RepoDigests 0}}' rust:1.82.0-slim
# and replace the tag below with rust:1.82.0-slim@sha256:<digest> (same for
# debian). The Rust version is also pinned in rust-toolchain.toml.

FROM rust:1.82.0-slim AS build
WORKDIR /app
COPY . .
RUN cargo build --workspace --release

# ---- reproduce: the one command runs this stage ----------------------------
FROM build AS reproduce
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 python3-matplotlib \
 && rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["bash", "docker/reproduce.sh"]

# ---- runtime: the mesh binary for the docker-compose production tier and the
#      Crane Cloud real-Internet tier ------------------------------------------
# The entrypoint runs the cloud collector by default (no arguments needed), so
# the image deploys on Crane Cloud as a single app with the entry command left
# blank; docker-compose still passes its own `command:` through unchanged.
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends iproute2 ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/mesh /usr/local/bin/mesh
COPY docker/mesh-entrypoint.sh /usr/local/bin/mesh-entrypoint.sh
RUN chmod +x /usr/local/bin/mesh-entrypoint.sh
ENTRYPOINT ["/usr/local/bin/mesh-entrypoint.sh"]
