# PIR Server
# Top-level Makefile for local development
#
# Three server binaries:
#   - spend-server:   nullifier PIR only
#   - witness-server: witness PIR only
#   - pir-server:     combined (both in one process)
#
# Usage: make build && make run

# ── Configuration (override with env vars) ───────────────────────────
DATA_DIR  ?= ./data
LWD_URL   ?= https://zec.rocks:443
LISTEN    ?= 0.0.0.0:8080

# ── Targets ──────────────────────────────────────────────────────────

.PHONY: build build-nullifier build-witness build-combined \
        run run-nullifier run-witness run-combined \
        test test-ypir clean help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

build: build-combined ## Build combined pir-server (default)

build-nullifier: ## Build spend-server binary (nullifier only)
	cargo build --release -p spend-server --features ypir

build-witness: ## Build witness-server binary (witness only)
	cargo build --release -p witness-server --features ypir

build-combined: ## Build combined pir-server binary
	cargo build --release -p combined-server --features ypir

run: run-combined ## Run combined pir-server (default)

run-nullifier: ## Run spend-server locally
	cargo run --release -p spend-server --features ypir -- \
		--lwd-url $(LWD_URL) \
		--data-dir $(DATA_DIR) \
		--listen $(LISTEN)

run-witness: ## Run witness-server locally
	cargo run --release -p witness-server --features ypir -- \
		--lwd-url $(LWD_URL) \
		--data-dir $(DATA_DIR) \
		--listen $(LISTEN)

run-combined: ## Run combined pir-server locally
	cargo run --release -p combined-server --features ypir -- \
		--lwd-url $(LWD_URL) \
		--data-dir $(DATA_DIR) \
		--listen $(LISTEN)

test: ## Run workspace tests (no YPIR)
	cargo test --workspace

test-ypir: ## Run all tests including YPIR round-trips (release mode)
	cargo test --workspace --all-features --release

clean: ## Remove build artifacts and data files
	cargo clean
	rm -rf $(DATA_DIR)
