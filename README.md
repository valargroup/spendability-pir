# Spendability PIR

Private nullifier spendability checks for Zcash wallets using single-server Private Information Retrieval (PIR). A wallet can instantly determine if its notes are spendable — privately, with sub-second latency, no sync — by issuing a single encrypted YPIR query over a bucketed hash table of recent Orchard nullifiers.

## Architecture

```
lightwalletd ──gRPC──> nf-ingest ──ChainEvent──> HashTableDb ──to_pir_bytes──> YPIR Engine
                                                                                    │
                                                                              ArcSwap (atomic)
                                                                                    │
                                                  Wallet ──HTTP──> spend-server ────┘
                                                    │
                                                    └── SpendClient::is_spent(nf) -> bool
```

The server ingests Orchard nullifiers from lightwalletd, stores them in a bucketed hash table with per-block tracking, and serves encrypted PIR queries via YPIR SimplePIR. The client generates a private query for the bucket containing its nullifier, sends it to the server, and decodes the encrypted response locally.

## Crates

| Crate | Description |
|-------|-------------|
| `spend-types` | Shared constants, types (`ChainEvent`, `SpendabilityMetadata`), and `PirEngine` trait |
| `hashtable-pir` | Bucketed hash table with per-block insert/rollback, LRU eviction, crash-safe snapshots |
| `nf-ingest` | lightwalletd gRPC client, compact block parsing, reorg detection, sync/follow streams |
| `spend-server` | Axum HTTP server with sync/follow modes, YPIR serving, async snapshots, atomic PIR swap |
| `spend-client` | `SpendClient` with `is_spent(nf)` API using YPIR SimplePIR |

## Quick Start

### Build

```bash
# Library only (no YPIR dependency)
cargo build

# With YPIR (builds the server binary)
cargo build --features ypir -p spend-server
```

### Run

```bash
cargo run -p spend-server --features ypir --release -- \
    --lwd-url http://localhost:9067 \
    --data-dir ./data \
    --listen 0.0.0.0:8080
```

The server will:
1. Connect to lightwalletd at the given endpoint
2. Sync recent blocks to fill the hash table (~1M nullifiers)
3. Build the YPIR database (~3s)
4. Start serving HTTP queries

During sync, `GET /health` reports progress and `POST /query` returns 503.

### Configuration

| Flag | Default | Description |
|------|---------|-------------|
| `--lwd-url` | (required) | lightwalletd gRPC endpoint, repeatable for fallback |
| `--data-dir` | `./data` | Directory for snapshots and hint cache |
| `--listen` | `0.0.0.0:8080` | HTTP listen address |
| `--target-size` | `1000000` | Max nullifiers before oldest-block eviction |
| `--snapshot-interval` | `100` | Blocks between crash-safe snapshots |

### HTTP API

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Server status, phase, height, nullifier count |
| `/metadata` | GET | `SpendabilityMetadata` JSON (503 during sync) |
| `/params` | GET | `YpirScenario` JSON (always available) |
| `/query` | POST | YPIR query bytes in, encrypted response out (503 during sync) |

## Testing

```bash
# Fast tests (no YPIR, ~10s)
cargo test --workspace

# Full tests including YPIR round-trips (~3min, release mode recommended)
cargo test --workspace --all-features --release
```

## Parameters

| Parameter | Value | Notes |
|-----------|-------|-------|
| `NUM_BUCKETS` | 16,384 (2^14) | Hash table rows |
| `BUCKET_CAPACITY` | 112 | Max entries per bucket |
| `ENTRY_BYTES` | 32 | Nullifier size |
| `BUCKET_BYTES` | 3,584 | Row size (= SimplePIR minimum, zero padding) |
| `DB_BYTES` | ~56 MB | Total PIR database |
| `TARGET_SIZE` | 1,000,000 | Nullifiers before eviction |
| `CONFIRMATION_DEPTH` | 10 | Blocks before finalization |

### Performance (release mode)

| Metric | Value |
|--------|-------|
| PIR rebuild | ~3s |
| Server online (per query) | ~65ms |
| Client decode | ~6ms |
| Query upload | 672 KB |
| Response download | 12 KB |

## License

See individual crate licenses.
