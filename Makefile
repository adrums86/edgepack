# edgepack — Build targets for CDN edge deployment
#
# Targets:
#   make build              — Default WASI P2 component (wasmtime, Spin, Akamai)
#   make build-fastly       — Fastly Compute
#   make build-docker       — Docker image with wasmtime serve (AWS, self-hosted)
#   make build-all          — All platform targets
#   make test               — Run all tests on native host
#   make bench              — Run benchmarks

CARGO := cargo
HOST_TARGET := $(shell rustc -vV | grep host | awk '{print $$2}')
WASM_TARGET := wasm32-wasip2
WASM_BINARY := target/$(WASM_TARGET)/release/edgepack.wasm
DOCKER_IMAGE := edgepack
DOCKER_TAG := latest

.PHONY: build build-fastly build-docker build-all \
        test bench clean check

# ---------------------------------------------------------------------------
# Default WASI P2 component
# Works with: wasmtime serve, Fermyon Spin, Akamai (via Fermyon), SpinKube,
#             Cloudflare Workers
# ---------------------------------------------------------------------------
build:
	$(CARGO) build --release --target $(WASM_TARGET)
	@echo "Built: $(WASM_BINARY)"
	@ls -lh $(WASM_BINARY) | awk '{print "Size:", $$5}'

# ---------------------------------------------------------------------------
# Fastly Compute
# Requires: fastly CLI (https://developer.fastly.com/learning/tools/cli)
# ---------------------------------------------------------------------------
build-fastly:
	$(CARGO) build --release --target $(WASM_TARGET)
	@echo "Built for Fastly: $(WASM_BINARY)"
	@echo "Deploy: cd deploy/fastly && fastly compute deploy"

# ---------------------------------------------------------------------------
# Docker image (wasmtime serve)
# Works with: AWS ECS/Fargate, Lambda container, any container host
# Use behind CloudFront, Akamai, or any CDN as origin
# ---------------------------------------------------------------------------
build-docker: build
	docker build -t $(DOCKER_IMAGE):$(DOCKER_TAG) -f deploy/docker/Dockerfile .
	@echo "Built Docker image: $(DOCKER_IMAGE):$(DOCKER_TAG)"

# ---------------------------------------------------------------------------
# All platforms
# ---------------------------------------------------------------------------
build-all: build build-docker

# ---------------------------------------------------------------------------
# Test & bench (native host target)
# ---------------------------------------------------------------------------
test:
	$(CARGO) test --target $(HOST_TARGET) --features ts

check:
	$(CARGO) check --target $(WASM_TARGET)
	$(CARGO) check --target $(WASM_TARGET) --features ts

bench:
	$(CARGO) bench --target $(HOST_TARGET)

# ---------------------------------------------------------------------------
# Sandbox (local development)
# ---------------------------------------------------------------------------
sandbox:
	$(CARGO) run --bin sandbox --features sandbox --target $(HOST_TARGET)

# ---------------------------------------------------------------------------
# Clean
# ---------------------------------------------------------------------------
clean:
	$(CARGO) clean
