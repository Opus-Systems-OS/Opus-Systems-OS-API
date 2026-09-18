# Opus Systems OS API — deployment image. Build context is the repo root:
#   docker build -f api/Dockerfile .
# .github/workflows/image.yml does this on every push to main and pushes to
# GHCR; the droplet only ever pulls (1 vCPU / 961 MB cannot do a Rust build).

# ---- build ---------------------------------------------------------------
FROM rust:1-bookworm AS build
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
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /src/target/release/opus-api /usr/local/bin/opus-api
# Runs as root on purpose: the Docker named volume for /data is root-owned.
ENV RUST_LOG=info,tower_http=info
EXPOSE 8100
CMD ["opus-api", "serve"]
