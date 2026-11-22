# Zapslab — containerized builds.
#
# Every cargo/rust invocation runs inside a pinned builder image, so the host
# only needs a container runtime (nerdctl / docker / podman) and make.

RUNTIME ?= $(shell command -v nerdctl >/dev/null 2>&1 && echo nerdctl || (command -v docker >/dev/null 2>&1 && echo docker || echo podman))
IMAGE ?= zapslab-builder
# Default to the Rust devcontainer image already present on this machine
# (rustc 1.99.1). For fully reproducible builds elsewhere, use:
#   make image RUST_IMAGE=rust:1.98.1-bookworm
RUST_IMAGE ?= mcr.microsoft.com/devcontainers/rust:1-bookworm
WASM_BINDGEN_VERSION ?= 0.2.127
# Pinned wgpu revision (zed's workspace pin) that vendor-wgpu patches.
WGPU_REV ?= cddb2f5d80dc0009b2ef6767635140cacf90a007

CACHE := $(CURDIR)/.cache
CARGO_HOME_DIR := $(CACHE)/cargo
HOME_DIR := $(CACHE)/home
WGPU_PATCHED := $(CACHE)/wgpu-patched

# Runtime invocation: source mounted at /app, caches kept inside the project
# (gitignored). This runtime maps container root to the invoking host user, so
# files stay host-owned without a --user flag.
RUN = $(RUNTIME) run --rm \
	-v "$(CURDIR)":/app \
	-w /app \
	-e CARGO_HOME=/app/.cache/cargo \
	-e HOME=/app/.cache/home \
	-e CARGO_NET_GIT_FETCH_WITH_CLI=true \
	$(IMAGE)

.DEFAULT_GOAL := help
.PHONY: help image check test wasm web icons serve shell vendor-wgpu clean

help:
	@echo "Zapslab targets:"
	@echo "  image  build the builder image ($(IMAGE) from $(RUST_IMAGE))"
	@echo "  check  cargo check for wasm32-unknown-unknown (fast feedback)"
	@echo "  test   unit tests for the pure document model (no wasm/gpui deps)"
	@echo "  wasm   release wasm build (no packaging)"
	@echo "  web    full pipeline: wasm + wasm-bindgen + web assets -> dist/"
	@echo "  icons  render PWA icons from scripts/zapslab-icon-64.png (host python3 + pillow)"
	@echo "  serve  serve dist/ on http://localhost:8119 (host python3)"
	@echo "  shell  interactive shell inside the builder image"
	@echo "  vendor-wgpu  materialize the patched wgpu source (Firefox/WebGL2 fix)"
	@echo "  clean  remove target/, dist/ and .cache/"
	@echo ""
	@echo "current runtime: $(RUNTIME) (override with RUNTIME=docker|nerdctl|podman)"

$(CARGO_HOME_DIR) $(HOME_DIR):
	mkdir -p $@

image:
	$(RUNTIME) build \
		--build-arg RUST_IMAGE=$(RUST_IMAGE) \
		--build-arg WASM_BINDGEN_VERSION=$(WASM_BINDGEN_VERSION) \
		-t $(IMAGE) docker

check: $(CARGO_HOME_DIR) $(HOME_DIR) vendor-wgpu
	$(RUN) cargo check --target wasm32-unknown-unknown

test: $(CARGO_HOME_DIR) $(HOME_DIR)
	$(RUN) cargo test --manifest-path crates/document/Cargo.toml

wasm: $(CARGO_HOME_DIR) $(HOME_DIR) vendor-wgpu
	$(RUN) cargo build --profile web --target wasm32-unknown-unknown

web: $(CARGO_HOME_DIR) $(HOME_DIR) vendor-wgpu
	$(RUN) sh scripts/build_web.sh

icons:
	python3 scripts/gen_icons.py

serve: web
	python3 -m http.server 8119 --directory dist

shell: $(CARGO_HOME_DIR) $(HOME_DIR)
	$(RUNTIME) run --rm -it \
		-v "$(CURDIR)":/app \
		-w /app \
		-e CARGO_HOME=/app/.cache/cargo \
		-e HOME=/app/.cache/home \
		-e CARGO_NET_GIT_FETCH_WITH_CLI=true \
		$(IMAGE) bash

vendor-wgpu:
	@if [ ! -d $(WGPU_PATCHED)/wgpu-core ]; then \
		echo "vendoring wgpu $(WGPU_REV)..."; \
		git init -q $(WGPU_PATCHED); \
		git -C $(WGPU_PATCHED) remote add origin https://github.com/gfx-rs/wgpu; \
		git -C $(WGPU_PATCHED) fetch -q --depth 1 origin $(WGPU_REV); \
		git -C $(WGPU_PATCHED) checkout -q FETCH_HEAD; \
		sed -i 's/pub(crate) const ZERO_BUFFER_SIZE: BufferAddress = 512 << 10;/pub(crate) const ZERO_BUFFER_SIZE: BufferAddress = 4 << 20;/' $(WGPU_PATCHED)/wgpu-core/src/device/mod.rs; \
		grep -q 'ZERO_BUFFER_SIZE: BufferAddress = 4 << 20;' $(WGPU_PATCHED)/wgpu-core/src/device/mod.rs || { echo "error: ZERO_BUFFER_SIZE patch did not apply (wgpu source changed?)" >&2; rm -rf $(WGPU_PATCHED); exit 1; }; \
	fi
	@echo "wgpu patched source ready: $(WGPU_PATCHED)"

clean:
	rm -rf target dist .cache
