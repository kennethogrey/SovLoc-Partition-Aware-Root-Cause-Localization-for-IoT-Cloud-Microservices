# SovLoc is built and run entirely inside Docker, so the only thing a grader
# needs on the host is Docker with Compose v2. There are three stages.
#
#   build      compiles the whole Rust workspace once.
#   reproduce  runs the full reproduction (build, unit tests, record the
#              environment, the seeded experiments, then the statistics, tables,
#              numbers.tex and the SVG figure). This is what the one command uses.
#              It is Rust only: no Python, no TeX.
#   runtime    a small image that ships only the mesh binary plus tc (iproute2)
#              so docker-compose and the k3s/OpenStack/Crane Cloud tiers can stand
#              up the real mesh and a real network partition for the production tier.
#
# Base images are pinned by digest (examiner "Reproducibility statement": pinned
# container digests), in addition to the tag and to rust-toolchain.toml. To refresh
# a digest after a deliberate base bump:
#   docker pull rust:1.82.0-slim && docker inspect \
#     --format='{{index .RepoDigests 0}}' rust:1.82.0-slim

FROM rust:1.82.0-slim@sha256:1111c28d995d06a7863ba6cea3b3dcb87bebe65af8ec5517caaf2c8c26f38010 AS build
WORKDIR /app
COPY . .
RUN cargo build --workspace --release

# ---- reproduce: the one command runs this stage ----------------------------
FROM build AS reproduce
ENTRYPOINT ["bash", "docker/reproduce.sh"]

# ---- runtime: the mesh binary for the docker-compose production tier and the
#      k3s / OpenStack / Crane Cloud tiers -------------------------------------
# The entrypoint runs the cloud collector by default (no arguments needed), so
# the image deploys on Crane Cloud as a single app with the entry command left
# blank; docker-compose still passes its own `command:` through unchanged.
FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587 AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends iproute2 ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/mesh /usr/local/bin/mesh
# Ship the example scenarios in the image too, so a managed deployment (for
# example Crane Cloud, which has no local volume to mount) can run a configured
# topology with `--scenario /app/scenarios/<file>.yaml`. The default collector and
# service ignore these unless a scenario is passed, so this changes nothing for
# the existing single-topology deployment.
COPY --from=build /app/scenarios /app/scenarios
COPY docker/mesh-entrypoint.sh /usr/local/bin/mesh-entrypoint.sh
RUN chmod +x /usr/local/bin/mesh-entrypoint.sh
ENTRYPOINT ["/usr/local/bin/mesh-entrypoint.sh"]
