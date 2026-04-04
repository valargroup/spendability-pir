# PIR-Assisted Note Commitment Witness Construction

## Goal

Enable notes to be **spendable immediately upon discovery during sync**, before the wallet has finished scanning. Once the wallet is fully synced, it has sufficient local data (ShardTree) to construct witnesses on its own — PIR is not needed for a caught-up wallet. The PIR witness system bridges the gap between note discovery and sync completion.

## Problem

When a note is discovered during sync, two blockers prevent spending:

1. **Shard tree completeness**: The shard (2^16 = 65,536 leaves, height 16) containing the note must be fully scanned before `witness_at_checkpoint_id_caching()` can produce a Merkle path.
2. **Unscanned tip gate**: `unscanned_tip_exists()` blanket-rejects all notes from `select_spendable_notes` if any unscanned ranges exist up to the anchor height.

These create a window (30s–1min or longer, depending on how far behind the wallet is) where a discovered note cannot be spent. The PIR witness system eliminates this window by providing the authentication path from an external server, bypassing the need for local shard completion.

### Witness lifecycle

1. **During sync** (PIR active): Note discovered at position P. The wallet's ShardTree is incomplete — it cannot produce a witness. The wallet queries the PIR server, which returns a complete authentication path at anchor height H. The wallet stores this witness and marks the note spendable.
2. **Wallet catches up**: Sync completes. The ShardTree now has the full shard and can produce witnesses locally. The PIR-obtained witness remains valid and usable — there is no need to re-derive it.
3. **Steady state** (PIR unnecessary): The wallet is synced. New notes arrive near the tip. The ShardTree can produce witnesses directly from locally-scanned data. PIR is not queried.

PIR is a **sync-time accelerator**, not a permanent replacement for the ShardTree. The server needs to maintain tree data for the historical range that wallets are likely to be syncing through, not necessarily the absolute tip.

## Key data: shard geometry and 6-month volume

Orchard uses depth 32 with `SHARD_HEIGHT = 16`, so each shard covers 2^16 = 65,536 leaves. Sapling is out of scope — its volume is declining (~13K notes/month vs Orchard's ~100K/month) and the pool is expected to sunset.

At the current mainnet rate of ~3,465 Orchard notes/day (measured April 2026):

- One shard fills in **~19 days**
- 6-month window: **~623,694 notes = ~9.5 shards**
- Total leaf data for 6 months: 623,694 × 32 = **~19.5 MB**
- Total populated shards on mainnet: **~761 completed + 1 frontier**

| Period (heights) | Orchard notes | Sapling notes | Orchard/day |
|------------------|--------------|---------------|-------------|
| 3,089,134 → 3,123,694 | 129,776 | 15,732 | 4,326 |
| 3,123,694 → 3,158,254 | 144,472 | 14,305 | 4,816 |
| 3,158,254 → 3,192,814 | 84,360 | 12,687 | 2,812 |
| 3,192,814 → 3,227,374 | 83,549 | 12,455 | 2,785 |
| 3,227,374 → 3,261,934 | 103,900 | 12,841 | 3,463 |
| 3,261,934 → 3,296,494 | 77,637 | 12,653 | 2,588 |
| **Total** | **623,694** | **80,673** | **~3,465 avg** |

The small 6-month volume makes a three-tier design unnecessary. The design uses a single broadcast + single PIR tier. L0 is sized at 8,192 rows (32 shards, ~2.1M notes, 64 MB), covering ~1.7 years at current rates.

## Design: Broadcast + Single-Tier PIR Witness Service

A server maintains the full Orchard note commitment tree (depth 32). The tree is decomposed at two depths, but only the lowest tier uses PIR:

```
Depth 0 (root)
  |
  | Broadcast — cap (shard roots array)
  | 16 levels, ~762 populated shard roots on mainnet
  | ~24 KB (762 x 32 bytes); client rebuilds cap tree locally
  |
Depth 16 (shard roots)
  |
  | Broadcast — sub-shard roots for active window
  | 8 levels within each shard, 256 sub-shard roots per shard
  | Up to 32 shards x 256 x 32 = ~256 KB at capacity
  |
Depth 24 (sub-shard roots)
  |
  | PIR — leaf commitments per sub-shard
  | 8 levels, 256 leaves per sub-shard
  | 8,192 rows (padded) x 8 KB = ~64 MB database
  |
Depth 32 (note commitments)
```

### Tier parameters

- **Broadcast (cap + sub-shard roots)**: Downloaded periodically by the client (cached, refreshed when stale or on verification failure).
  - Cap: the server broadcasts all populated shard roots as an array (~762 × 32 = ~24 KB). The client rebuilds the depth-16 cap tree locally: populated positions get shard roots, the rest are `MerkleHashOrchard::empty_root(Level::from(16))`. Only ~762 leaves need hashing — ~12K Sinsemilla operations, well under 50ms on mobile.
  - Sub-shard roots for active window: ~80 KB at 10 shards (6-month window), up to ~256 KB at 32 shards.
  - **Total broadcast: ~104 KB initially, up to ~280 KB at capacity.** Provides **24 of 32** authentication path siblings.
- **PIR tier (sub-shard leaf data)**: One row per populated sub-shard, padded to 8,192 rows.
  - Row size: 256 leaves × 32 bytes = 8,192 bytes
  - **Database: 64 MB** (8,192 rows × 8 KB, padded regardless of fill level)
  - **Bandwidth: ~605 KB per query** (single PIR round trip, `nu_1 = 0`)
  - Provides the final **8 of 32** authentication path siblings.
  - Capacity: 32 shards = ~2.1M notes = ~1.7 years at current rates.
  - **Row layout**: Rows are dense within the window. The PIR database covers a contiguous shard range `[window_start_shard, window_start_shard + window_shard_count)`. Physical PIR row index = `(shard_index - window_start_shard) * 256 + subshard_index`. Rows beyond the populated range are zero-filled. The broadcast includes `window_start_shard` and `window_shard_count`.

### Client witness reconstruction

Given a note at tree position P (32-bit):

```
shard_index    = P >> 16          (which shard — top 16 bits)
subshard_index = (P >> 8) & 0xFF  (which sub-shard within shard — middle 8 bits)
leaf_index     = P & 0xFF         (which leaf within sub-shard — bottom 8 bits)
```

1. **Broadcast data** (cached): Extract 16 cap siblings using `shard_index`, then 8 upper intra-shard siblings using `subshard_index` from the broadcast sub-shard roots. Verify: hash of 256 sub-shard roots matches the shard root from cap.
2. **PIR query** for physical row `(shard_index - window_start_shard) * 256 + subshard_index`: Receive 256 leaf commitments. Build local 8-level tree, extract 8 lower siblings using `leaf_index`. Verify: hash of 256 leaves matches sub-shard root from broadcast.
3. **Assemble**: 16 + 8 + 8 = 32 sibling hashes = complete Merkle authentication path.
4. **Self-verify**: Hash the note commitment through all 32 siblings. Result must equal the anchor root (publicly known from the chain). This catches server errors or malicious data.

### Anchor depth and confirmation policy

The witness server uses `CONFIRMATION_DEPTH = 10`, shared with the nullifier PIR server via `shared/pir-types`.

The wallet's `ConfirmationsPolicy` has three settings — transfer trusted (3), transfer untrusted (10), shielding (1). The wallet picks a **single anchor per transaction** using the trusted value (`tip - 3`) and filters individual notes by their confirmation depth. The PIR server serves witnesses at one anchor height (`tip - 10`), and the wallet uses whatever anchor the server provides. An anchor at `tip - 10` satisfies all policies since it's deeper than all of them. Using 10 also provides reorg safety.

One PIR database at one anchor height serves all confirmation policies. The 7-block difference between `tip - 3` and `tip - 10` (~9 minutes) is negligible — the PIR witness system targets wallets that are behind during sync. By the time the wallet reaches `tip - 10`, it can construct witnesses locally.

### Database update strategy

Follows the same per-block rebuild cycle as the nullifier PIR server: ingest each new block, update the tree, rebuild PIR, atomic swap via `ArcSwap`.

- **Completed shards**: Immutable once full (~every 19 days). Compute once, serve forever.
- **Frontier shard**: Updated every block. Only the active sub-shard row changes.
- **Broadcast data**: Regenerated alongside every PIR rebuild. Negligible cost.
- **Eviction**: Old sub-shard rows evicted when L0 exceeds 32 shards. At current volume this takes ~1.7 years.
- **PIR rebuild**: At 64 MB (padded), full YPIR setup takes ~3.5 seconds — well under the 75-second block interval.

### Scaling: LSM-style tiered PIR

At current volume, L0 covers ~1.7 years. If Orchard adoption increases, L0 fills faster. The solution borrows from LSM-trees: L0 absorbs new data with per-block rebuilds, and flushes completed shards into a cold tier (L1) that rebuilds infrequently.

|                    | L0 (hot)    | L1 (cold) |
| ------------------ | ----------- | --------- |
| `db_rows` (padded) | 8,192       | 131,072   |
| Shards capacity    | 32          | 512       |
| Notes capacity     | 2.1M        | 33.6M     |
| DB size            | 64 MB       | 1 GB      |
| Rebuild time       | ~3.5s       | ~55s      |
| Rebuild trigger    | Every block | L0 flush  |

| Volume       | Notes/day | Shard fill rate | L0 fills in | Active tiers |
| ------------ | --------- | --------------- | ----------- | ------------ |
| 1x (current) | ~3,500    | ~19 days        | ~1.7 years  | L0 only      |
| 5x           | ~17,500   | ~4 days         | ~4 months   | L0 + L1      |
| 10x          | ~35,000   | ~2 days         | ~2 months   | L0 + L1      |

V1 builds L0 only. The `commitment-tree-db` API tracks completed vs. frontier shard membership so `build_pir_db()` can later become per-tier builders without architectural changes.

## Workspace layout

Phase 0 (prerequisite): Restructure the current flat `sync-nullifier-pir/` workspace into a parent workspace with two sub-workspaces. The nullifier and witness systems are separate packages that share common dependencies.

```
sync-nullifier-pir/
├── Cargo.toml                # parent workspace, defines [workspace] members + shared deps
├── proto/                    # shared: compact_formats.proto, service.proto
├── shared/
│   ├── pir-types/            # PirEngine trait, YpirScenario, ServerPhase (extracted from spend-types)
│   └── chain-ingest/         # LwdClient, ChainTracker, sync/follow streams (extracted from nf-ingest)
├── nullifier/
│   ├── spend-types/          # nullifier-specific constants (NUM_BUCKETS, BUCKET_BYTES, hash_to_bucket, etc.)
│   ├── hashtable-pir/
│   ├── nf-ingest/            # nullifier extraction (parser module), depends on chain-ingest
│   ├── spend-server/
│   └── spend-client/
├── witness/
│   ├── witness-types/
│   ├── commitment-ingest/    # note commitment extraction, depends on chain-ingest
│   ├── commitment-tree-db/
│   ├── witness-server/
│   └── witness-client/
├── combined-server/          # optional: single binary running both servers in-process
└── note-witness/             # analysis / design docs (this file)
```

### Shared crate extraction

- `shared/pir-types` — extracted from `spend-types`: `PirEngine` trait, `YpirScenario`, `ServerPhase`, `NU5_MAINNET_ACTIVATION`, `CONFIRMATION_DEPTH`. What stays in `nullifier/spend-types`: `NUM_BUCKETS`, `BUCKET_CAPACITY`, `ENTRY_BYTES`, `BUCKET_BYTES`, `DB_BYTES`, `hash_to_bucket`, `ChainEvent`, `NewBlock`, `OrphanedBlock`, `SpendabilityMetadata`.
- `shared/chain-ingest` — extracted from `nf-ingest`: `LwdClient` (`client.rs`), `ChainTracker`/`ChainAction` (`chain_tracker.rs`), proto types (`proto.rs` + generated code). What stays in `nullifier/nf-ingest`: `parser.rs` (`extract_nullifiers`), `ingest.rs` (nullifier-specific sync/follow). The witness-side `commitment-ingest` writes its own sync/follow loop using `LwdClient` + `ChainTracker`, extracting `CompactOrchardAction.cmx` instead of nullifiers.

### Workspace dependencies

```toml
[workspace]
resolver = "2"
members = [
    "shared/pir-types", "shared/chain-ingest",
    "nullifier/spend-types", "nullifier/hashtable-pir", "nullifier/nf-ingest",
    "nullifier/spend-server", "nullifier/spend-client",
    "witness/witness-types", "witness/commitment-ingest", "witness/commitment-tree-db",
    "witness/witness-server", "witness/witness-client",
    "combined-server",
]

[workspace.dependencies]
ypir = { git = "https://github.com/valargroup/ypir.git", branch = "valar/artifact", default-features = false }
spiral-rs = { git = "https://github.com/valargroup/spiral-rs.git", branch = "valar/avoid-avx512" }
tonic = "0.12"
prost = "0.13"
tokio = { version = "1", features = ["full"] }
axum = "0.7"
arc-swap = "1"
```

### Deployment modes

1. **Separate binaries** (default): `nullifier/spend-server` and `witness/witness-server` each have a `[[bin]]` target and run as independent processes. Each connects to lightwalletd independently. Two systemd services, each with its own port.
2. **Combined in-process**: `combined-server/` depends on both as libraries and runs both on a single Axum router (route prefixes: `/nullifier/...` and `/witness/...`). Single process, single lightwalletd connection, single port. The `chain-ingest` streams feed both the hash table and the commitment tree from the same compact block flow, halving lightwalletd load.

Both modes produce identical client-facing APIs.

### Migration path

Phase 0 reorganizes the existing crates into `nullifier/` without changing any code — just moves directories and updates `Cargo.toml` paths. All existing tests pass. Then witness crates are built in `witness/` incrementally. The combined server is added last, after both systems work independently.

## Server crates (`witness/`)

- **`witness-types`**: Constants (`TREE_DEPTH=32`, `SHARD_HEIGHT=16`, `SUBSHARD_HEIGHT=8`, `SUBSHARD_LEAVES=256`), anchor tracking. Key types:
  - `PirWitness { position: Position, siblings: [MerkleHashOrchard; 32], anchor_height: BlockHeight, anchor_root: MerkleHashOrchard }` — complete witness bundle. Contains everything needed to convert to `MerklePath<MerkleHashOrchard>` and to verify/store in the wallet. `position` determines left/right merge direction at each level; `anchor_root` is for self-verification.
  - `CapData` — serialized shard roots for cap tree reconstruction.
  - `BroadcastData` — cap + sub-shard roots + `window_start_shard` + `window_shard_count` + `anchor_height`.
- **`commitment-ingest`**: Depends on `chain-ingest` for `LwdClient`, `ChainTracker`. Extracts Orchard note commitments (`CompactOrchardAction.cmx`). Feeds them into the tree builder.
- **`commitment-tree-db`**: In-memory Merkle tree. Internal nodes use `MerkleHashOrchard::combine(level, left, right)` (Sinsemilla hash); empty subtrees use `MerkleHashOrchard::empty_root(level)` (level-dependent). Requires the `orchard` crate. Key operations:
  - `append_commitments(height, commitments)` — extend the tree
  - `rollback_to(height)` — handle reorgs
  - `shard_roots() -> Vec<(shard_idx, Hash)>` — for cap construction
  - `subshard_roots(shard_idx) -> [Hash; 256]` — broadcast data
  - `subshard_leaves(shard_idx, subshard_idx) -> [Hash; 256]` — PIR row data. Empty leaf positions beyond the frontier are `MerkleHashOrchard::empty_root(Level::from(0))`, not zero bytes.
  - `build_pir_db() -> Vec<u8>` — row-major bytes for YPIR setup. Rows within the populated window use `subshard_leaves()` (proper empty-leaf sentinels). Padding rows beyond the window are zero-filled (never queried).
  - Snapshot/restore
- **`witness-server`**: Axum HTTP server:
  - `GET /health` — liveness
  - `GET /metadata` — anchor height, tree size, epoch, `window_start_shard`, `window_shard_count`
  - `GET /broadcast` — shard roots array + sub-shard roots for active window (~104–280 KB). Client rebuilds cap tree locally.
  - `GET /params` — YPIR parameters
  - `POST /query` — YPIR query against the sub-shard leaf database
  - Exposes `build_router()` as a library function so the combined server can mount it
- **`witness-client`**: Client library:
  - `WitnessClient::connect(url)` — fetch params, initialize `YPIRClient`
  - `get_broadcast()` — download and cache cap + sub-shard roots
  - `get_witness(position) -> Result<PirWitness, WitnessError>` — checks position is within the server's window, issues PIR query, reconstructs path, self-verifies. Returns `WitnessError::NoteOutsideWindow` if the note's shard falls outside the window — wallet falls back to local scan.
  - `WitnessClientBlocking` — sync wrapper for FFI

## End-to-end validation milestone

Before any wallet integration, the PIR witness system must pass an end-to-end test against real mainnet data.

**Test A: tree correctness (fast, stub PIR)**

Validates tree construction, sub-shard decomposition, and witness reconstruction without YPIR overhead.

1. `commitment-ingest` syncs the Orchard tree from lightwalletd up to target height H.
2. Pick a known Orchard note commitment at position P (from a compact block's `CompactOrchardAction.cmx`).
3. `commitment-tree-db` builds the tree, produces broadcast data and raw PIR row bytes.
4. Look up the sub-shard row directly by index (no YPIR). Reconstruct the 32-sibling authentication path.
5. Verify against `GetTreeState(H)`: deserialize `orchardTree` frontier, call `.root()`, confirm the authentication path produces this root.
6. Cross-validate against a local `ShardTree`: confirm paths match byte-for-byte.

**Test B: full PIR round-trip (slow, YPIR)**

Same as Test A but through the real YPIR pipeline: server builds database, client generates encrypted query, server answers, client decodes. Validates PIR layer preserves correctness.

**What these prove**: tree construction correctness, sub-shard decomposition, witness reconstruction, anchor root agreement with chain state, `PirWitness` compatibility with `ShardTree`, and (Test B) YPIR encode/decode fidelity.

## Wallet integration (future scope)

Wallet-side changes are out of scope for the initial PIR witness server. The following integration points define the compatibility contract.

1. **PirWitness → MerklePath conversion**: `PirWitness { position, siblings, anchor_height, anchor_root }` converts to `MerklePath { position, path: siblings.to_vec() }`. The `anchor_height` and `anchor_root` are metadata for wallet storage and verification.
2. **Anchor root verification**: Computed root must agree with `root_at_checkpoint_id(&H)` from ShardTree and with `GetTreeState`.
3. **Note position**: `commitment_tree_position` (type `Position`) decomposes as `shard_index = P >> 16`, `subshard_index = (P >> 8) & 0xFF`, `leaf_index = P & 0xFF`. Physical PIR row = `(shard_index - window_start_shard) * 256 + subshard_index`.
4. **Gate bypass**: Same `skip_unscanned_check` pattern as nullifier PIR.
5. **Transaction builder injection**: Check `pir_witness_data` table first, fall back to `witness_at_checkpoint_id_caching`. The stored `PirWitness` provides `position`, `siblings`, `anchor_height`, and `anchor_root` — enough to construct `MerklePath<MerkleHashOrchard>` and the anchor.

Future wallet work: `pir_witness_data` table, `sync-witness-pir` feature flag, gate bypass, Rust FFI, Swift wrapper, `ScanAction` trigger — following the patterns established by nullifier PIR.

## Security properties

- **Privacy**: YPIR guarantees the server learns nothing about which position was queried.
- **Correctness**: Client self-verifies every witness against the publicly-known anchor root.
- **Availability**: If the PIR server is down, the wallet falls back to normal shard scanning — spendability is delayed until sync completes, but never blocked. PIR is a performance optimization, not a correctness dependency.

## Data sizes summary (~3,465 notes/day)

- **Broadcast**: ~104 KB initially (24 KB cap + 80 KB sub-shard roots), up to ~280 KB at full L0 capacity (32 shards)
- **PIR database**: 64 MB (fixed)
- **PIR bandwidth per witness**: ~605 KB upload + 36 KB download
- **Total per witness**: ~641 KB PIR + broadcast (cached)

Nullifier PIR comparison: ~56 MB database / ~3.3 MB bandwidth. Witness PIR per-query bandwidth is ~5× smaller.

## Method (volume analysis)

The measurements use `orchardCommitmentTreeSize` from `ChainMetadata` in compact blocks via lightwalletd (`zec.rocks:443`), collected April 2026. Each Orchard action produces one nullifier and one note commitment, so tree size growth equals the number of Orchard actions.

| Metric | Value |
|--------|-------|
| Chain tip | 3,296,494 |
| NU5 activation | 1,687,104 |
| Cumulative Orchard notes | 49,876,639 |
| Cumulative Sapling notes | 73,890,312 |
