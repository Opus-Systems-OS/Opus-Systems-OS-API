# Opus Systems OS API — deployment image. Build context is the repo root:
#   docker build -f api/Dockerfile .
# .github/workflows/image.yml does this on every push to main and pushes to
# GHCR; the droplet only ever pulls (1 vCPU / 961 MB cannot do a Rust build).

# Base images are pinned by digest (the tag is kept for Dependabot, which
# bumps the digest in a PR).

# ---- build ---------------------------------------------------------------
FROM rust:1-bookworm@sha256:9181b56c2a21ab1d5e73cffe04fc762426d30aa7fac3c91141dd033f49a7fc8b AS build
WORKDIR /src

# Dependencies first, cached separately from source changes.
COPY Cargo.toml Cargo.lock ./
COPY api/Cargo.toml api/Cargo.toml
RUN mkdir -p api/src \
 && echo 'fn main() {}' > api/src/main.rs \
 && echo '' > api/src/lib.rs \
 && cargo build --release -p opus-api \
 # Drop the dummy crate's artifacts *and* its fingerprint: COPY keeps source
 # mtimes, so the real files can look older than the dummy build and cargo
 # would otherwise consider the empty lib fresh.
 && rm -rf api/src target/release/deps/opus_api-* target/release/deps/libopus_api-* \
           target/release/.fingerprint/opus-api-*

COPY api/src api/src
RUN cargo build --release -p opus-api

# ---- runtime -------------------------------------------------------------
FROM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3
# `opus` (10001) owns /data, so a fresh named volume starts out owned by it.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --user-group --no-create-home --shell /usr/sbin/nologin opus \
 && mkdir -p /data && chown opus:opus /data
WORKDIR /app
COPY --from=build /src/target/release/opus-api /usr/local/bin/opus-api
# Still root by default: the droplet's existing api_data volume is
# root-owned and the Docker socket is root:docker, so `USER opus` here
# would stop the running deploy. Compose opts in with `user: "10001:10001"`
# once the volume is chowned and Docker sits behind a proxy — no
# chown-at-start step, which could not work under `cap_drop: ALL`.
ENV RUST_LOG=info,tower_http=info
EXPOSE 8100
CMD ["opus-api", "serve"]
