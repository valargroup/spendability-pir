# spend-server
# Top-level Makefile for local development
#
# The spend-server is an all-in-one binary: it syncs nullifiers from
# lightwalletd, builds the YPIR PIR database, and serves encrypted
# queries — all in a single process.
#
# Usage: make build && make run

# ── Configuration (override with env vars) ───────────────────────────
DATA_DIR  ?= ./data
LWD_URL   ?= https://zec.rocks:443
LISTEN    ?= 0.0.0.0:8080

# ── Targets ──────────────────────────────────────────────────────────

.PHONY: build run test test-ypir clean help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

build: ## Build spend-server binary (release, with YPIR)
	cargo build --release -p spend-server --features ypir

run: ## Run spend-server locally
	cargo run --release -p spend-server --features ypir -- \
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
