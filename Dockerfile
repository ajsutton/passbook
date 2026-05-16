# syntax=docker/dockerfile:1
#
# Passbook — one multi-stage build producing TWO final images:
#
#   --target reth-passbook      -> debian-slim image with the L1 binary
#   --target op-reth-passbook   -> debian-slim image with the OP binary
#
# HERMETIC BY CONSTRUCTION
# ------------------------
# The minimal-checkout fast path (scripts/seed-vendor.sh) writes a gitignored
# shallow `.vendor/optimism` mirror and a gitignored `.cargo/config.toml` that
# contains an ABSOLUTE host path. Both are excluded by .dockerignore, so the
# build context never carries host-local state. Instead the build stage runs
# `scripts/seed-vendor.sh` ITSELF: it shallow-clones the pinned optimism rev
# and regenerates `.cargo/config.toml` with an absolute path that is valid
# *inside the image* (/src/.vendor/optimism). The image is therefore
# reproducible purely from committed source — it never depends on, nor copies,
# anything the host generated.
#
# All cargo invocations are `--locked` (Cargo.lock is committed + load-bearing)
# and set CARGO_NET_GIT_FETCH_WITH_CLI=true so the system git binary handles
# the shallow optimism mirror and the paradigmxyz/reth git dependency (cargo's
# libgit2 mishandles shallow clones). The reth + op-reth compile from git is
# very large; a long build is expected, not a failure.

# ── build stage ─────────────────────────────────────────────────────────────
# rust:1.95.0-bookworm matches rust-toolchain.toml channel "1.95.0" exactly,
# so the pinned toolchain is the image's default toolchain (no rustup
# auto-install round trip needed; rust-toolchain.toml still pins it anyway).
FROM rust:1.95.0-bookworm AS build

WORKDIR /src

# git + ca-certificates : seed-vendor.sh shallow-clones the optimism rev and
#   cargo fetches the paradigmxyz/reth git dep + crates over HTTPS.
# build-essential + clang + pkg-config : the bundled SQLite C build for
#   `rusqlite` (features = ["bundled"]) needs a working C toolchain; clang and
#   pkg-config cover the broader reth native build surface (cc-rs based crates).
RUN apt-get update && apt-get install -y --no-install-recommends \
        git \
        ca-certificates \
        build-essential \
        clang \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Copy the committed working tree. .dockerignore excludes target/ .git/
# .vendor/ .cargo/ — so this brings ONLY source + Cargo.lock, never host state.
COPY . .

# Re-seed the vendor mirror + cargo config INSIDE the image. This shallow-
# clones the pinned optimism rev into /src/.vendor/optimism and writes
# /src/.cargo/config.toml with an absolute path valid in THIS image.
RUN CARGO_NET_GIT_FETCH_WITH_CLI=true bash scripts/seed-vendor.sh

# Build both release binaries. The registry/git cache mount only speeds up
# repeated builds; it never substitutes for the locked resolution, so it does
# not affect reproducibility (Cargo.lock + the pinned revs fully determine the
# graph). target/ is NOT a cache mount because the final stages COPY from it.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    CARGO_NET_GIT_FETCH_WITH_CLI=true \
    cargo build --release --locked -p reth-passbook -p op-reth-passbook

# ── final image: L1 (reth-passbook) ─────────────────────────────────────────
FROM debian:bookworm-slim AS reth-passbook
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/reth-passbook /usr/local/bin/reth-passbook
# Chain/DB data lives here; mount a host dir or named volume in production.
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/reth-passbook"]

# ── final image: OP (op-reth-passbook) ──────────────────────────────────────
FROM debian:bookworm-slim AS op-reth-passbook
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/op-reth-passbook /usr/local/bin/op-reth-passbook
VOLUME ["/data"]
ENTRYPOINT ["/usr/local/bin/op-reth-passbook"]
