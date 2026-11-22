# Builder image for Zapslab.
#
# Pinned Rust stable + wasm32 target + wasm-bindgen CLI (must match the
# wasm-bindgen pin in Cargo.toml) + binaryen for wasm-opt.
#
# Defaults to the locally cached Rust devcontainer image (rustc 1.99.x).
# Override for reproducibility, e.g.: make image RUST_IMAGE=rust:1.98.1-bookworm

ARG RUST_IMAGE=mcr.microsoft.com/devcontainers/rust:1-bookworm
FROM ${RUST_IMAGE}

ARG WASM_BINDGEN_VERSION=0.2.127

RUN rustup target add wasm32-unknown-unknown

RUN apt-get update \
 && apt-get install -y --no-install-recommends binaryen \
 && rm -rf /var/lib/apt/lists/*

RUN cargo install wasm-bindgen-cli --version ${WASM_BINDGEN_VERSION} --locked

# Large git dependencies (the zed repo) clone more reliably through git itself
# than through cargo's bundled libgit2.
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true
