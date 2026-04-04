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
- Total populated shards on mainnet: **~761 completed + 1 frontier** (= ~49.9M cumulative notes / 65,536 per shard)

| Period (heights) | Orchard notes | Sapling notes | Orchard/day |
|------------------|--------------|---------------|-------------|
| 3,089,134 → 3,123,694 | 129,776 | 15,732 | 4,326 |
| 3,123,694 → 3,158,254 | 144,472 | 14,305 | 4,816 |
| 3,158,254 → 3,192,814 | 84,360 | 12,687 | 2,812 |
| 3,192,814 → 3,227,374 | 83,549 | 12,455 | 2,785 |
| 3,227,374 → 3,261,934 | 103,900 | 12,841 | 3,463 |
| 3,261,934 → 3,296,494 | 77,637 | 12,653 | 2,588 |
| **Total** | **623,694** | **80,673** | **~3,465 avg** |

The Orchard commitment tree is **append-only** — notes are added sequentially from left to right, so the tree fills from position 0 onward. "Populated" means the shard contains at least one real note commitment: shards 0 through 760 are fully completed (all 65,536 leaf slots filled), shard 761 is the **frontier** (partially filled, currently receiving new notes), and shards 762 through 65,535 are completely empty. The count grows slowly — one new shard completes roughly every 19 days, adding 32 bytes to the cap broadcast. For the cap tree, the client fills unpopulated slots with `MerkleHashOrchard::empty_root(Level::from(16))` — a precomputed constant — so only the ~762 real roots contribute meaningful hashes.

The small 6-month volume makes a three-tier design unnecessary. The design uses a single broadcast + single PIR tier. L0 is sized at 8,192 rows (32 shards, ~2.1M notes, 64 MB), covering ~1.7 years at current rates.

## Design: Broadcast + Single-Tier PIR Witness Service

A server maintains the full Orchard note commitment tree (depth 32). The tree is decomposed at two depths, but only the lowest tier uses PIR:

```
Depth 0 (root)
  |
  | Broadcast — cap (shard roots array)
  | 16 levels, ~762 populated shard roots on mainnet
  | (the tree is append-only: shards 0..760 are completed,
  |  761 is the frontier, 762..65535 are empty)
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

### Why these split points

The split at **depth 16** is not arbitrary — it matches the Zcash protocol's `SHARD_HEIGHT = 16`, which is how `ShardTree` addresses leaves (`Position >> 16 = shard_index`). The PIR system decomposes the tree in exactly the same way the wallet does, so the `PirWitness` output is directly compatible with `MerklePath<MerkleHashOrchard>`.

The split at **depth 24** (creating 256-leaf sub-shards) balances three constraints:

1. **PIR row size**: 256 leaves × 32 bytes = 8 KB per row. Large enough that one PIR query returns a useful chunk of the tree, small enough for reasonable bandwidth (~605 KB per round trip).
2. **PIR database size**: 32 shards × 256 sub-shards = 8,192 rows × 8 KB = 64 MB. YPIR setup takes ~3.5 seconds at this size, which fits comfortably within the ~75-second block interval for per-block rebuilds.
3. **Broadcast efficiency**: The sub-shard roots (depth 16 → 24) are small enough to broadcast publicly — ~80 KB at 10 shards (6-month window), up to ~256 KB at full 32-shard capacity. This is a trivial download.

**Why not three tiers?** An alternative design would PIR-query both the sub-shard roots (depth 16 → 24) and the leaf commitments (depth 24 → 32). But with only ~10 shards in a 6-month window, the middle tier would have ~10 rows. PIR on 10 rows is wasteful — the query/response overhead (~600 KB) dwarfs the data. Broadcasting ~80 KB instead eliminates one entire PIR round trip with no privacy cost, since all clients receive identical broadcast data.

**Why this layout works well** — three properties of the Orchard tree make it amenable to this decomposition:

- **Append-only**: New leaves are only added at the frontier. Once a shard fills, its data never changes. Most of the PIR database is static — only one sub-shard row mutates per block.
- **Sparse at the top**: With ~762 shards out of 65,536 possible slots, the cap tree is mostly empty sentinels (precomputed constants). Broadcasting all real shard roots costs only ~24 KB.
- **Dense at the bottom**: Within a populated sub-shard, all 256 leaf slots contain real commitments (or the level-0 empty sentinel at the frontier). One PIR row provides everything needed to reconstruct 8 levels of the tree locally.

The net effect: **24 of 32 siblings come free** via broadcast, and only the bottom **8 require a private query**.

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

### Client query protocol

The client query process has a one-time initialization, a cached broadcast download, and a per-note PIR query.

**Initialization** (once per session): `WitnessClient::connect(url)` calls `GET /params` to fetch the `YpirScenario` JSON (PIR database geometry: 8,192 rows × 8,192 bytes). The client initializes a `YPIRClient` with these parameters. This is cached for the session.

**Broadcast download** (cached, refreshed when stale): `GET /broadcast` (~104 KB) returns cap data (all ~762 shard roots), sub-shard roots for the active PIR window, and metadata (`window_start_shard`, `window_shard_count`, `anchor_height`). This is a public, non-private download — all clients receive identical data. It is shared across all note queries in the session and only re-downloaded if the anchor height doesn't match a subsequent PIR response.

**Per-note PIR query**: For each note at position P, the client:

1. **Decomposes position P** (32-bit leaf index in the append-only tree):

```
shard_index    = P >> 16          (which shard — top 16 bits)
subshard_index = (P >> 8) & 0xFF  (which sub-shard within shard — middle 8 bits)
leaf_index     = P & 0xFF         (which leaf within sub-shard — bottom 8 bits)
```

2. **Validates the position** is within the PIR window: `shard_index` must be in `[window_start_shard, window_start_shard + window_shard_count)`. If not, returns `WitnessError::NoteOutsideWindow`.

3. **Computes the physical PIR row** index:

```
physical_row = (shard_index - window_start_shard) × 256 + subshard_index
```

4. **Generates an encrypted YPIR query** for `physical_row`: `ypir_client.generate_query(physical_row)` produces ~605 KB — 541 KB of fixed `pub_params` (encryption parameters, same regardless of which row) plus ~64 KB of `packed_query_row` (encrypted indicator vector encoding the target row). The server processes this against all 8,192 rows and cannot determine which row was requested.

5. **Sends the query** via `POST /query` (~605 KB upload). The server runs the YPIR online phase (~96ms), multiplying the query against all rows, and returns an encrypted response (~36 KB download).

6. **Decodes the response** locally to recover 256 leaf commitments (8,192 bytes = 256 × 32). Leaf positions beyond the tree's frontier are `MerkleHashOrchard::empty_root(Level::from(0))`, not zero bytes.

7. **Checks broadcast-to-PIR consistency**: If `broadcast.anchor_height ≠ query_response.anchor_height`, the server rebuilt the tree between the two requests. The client refetches the broadcast (~104 KB, cheap) and retries. For completed shards this never happens — their data is immutable regardless of anchor height. Only the frontier shard's active sub-shard can trigger a mismatch.

### Client witness reconstruction

Given the broadcast data and the decoded PIR response, the client reconstructs all 32 siblings locally:

1. **16 cap siblings** (from broadcast): Build a 16-level cap tree from the ~762 shard roots (unpopulated slots filled with `empty_root(Level::from(16))`). Walk from `shard_root[shard_index]` up to the root, recording the sibling at each level. Verify: hash of 256 sub-shard roots for this shard matches `shard_root[shard_index]` from the cap.

2. **8 sub-shard siblings** (from broadcast): Build an 8-level tree from the 256 sub-shard roots for `shard_index`. Walk from `subshard_root[subshard_index]` upward, recording 8 siblings.

3. **8 leaf siblings** (from PIR response): Build an 8-level tree from the 256 decoded leaf commitments. Walk from `leaf[leaf_index]` upward, recording 8 siblings. Verify: hash of 256 leaves matches `subshard_root[subshard_index]` from the broadcast.

4. **Assemble**: Concatenate siblings leaf-to-root: `siblings[0..7]` from leaves, `siblings[8..15]` from sub-shard roots, `siblings[16..31]` from cap. Total: 32 sibling hashes = complete Merkle authentication path.

5. **Self-verify** the complete path against the publicly-known anchor root:

```
node = note_commitment
for level in 0..32:
    sibling = siblings[level]
    if bit(P, level) == 0:
        node = MerkleHashOrchard::combine(level, node, sibling)   // left child
    else:
        node = MerkleHashOrchard::combine(level, sibling, node)   // right child
assert node == anchor_root
```

The `position` bits determine left/right direction at each level, and `combine` is the Sinsemilla hash via `MerkleHashOrchard::combine`. If verification passes, the witness is cryptographically valid — no trust in the server required. If it fails, the witness is discarded and not stored.

The result is a `PirWitness { position, siblings: [MerkleHashOrchard; 32], anchor_height, anchor_root }`.

### Anchor depth and confirmation policy

The witness server uses `CONFIRMATION_DEPTH = 10`, shared with the nullifier PIR server via `shared/pir-types`.

The wallet's `ConfirmationsPolicy` has three settings — transfer trusted (3), transfer untrusted (10), shielding (1). The wallet picks a **single anchor per transaction** using the trusted value (`tip - 3`) and filters individual notes by their confirmation depth. The PIR server serves witnesses at one anchor height (`tip - 10`), and the wallet uses whatever anchor the server provides. An anchor at `tip - 10` satisfies all policies since it's deeper than all of them. Using 10 also provides reorg safety.

One PIR database at one anchor height serves all confirmation policies. The 7-block difference between `tip - 3` and `tip - 10` (~9 minutes) is negligible — the PIR witness system targets wallets that are behind during sync. By the time the wallet reaches `tip - 10`, it can construct witnesses locally.

### Database update strategy

Follows the same per-block rebuild cycle as the nullifier PIR server: ingest each new block, update the tree, rebuild PIR, atomic swap via `ArcSwap`.

- **Completed shards**: Immutable once full (~every 19 days). Compute once, serve forever.
- **Frontier shard**: Updated every block. Only the active sub-shard row changes — at ~3 notes/block, that's ~96 bytes of new leaf data per block.
- **Broadcast data**: Regenerated alongside every PIR rebuild. Cap updates when a new shard completes (~every 19 days) or when the frontier hash changes (every block). Negligible cost.
- **PIR rebuild**: At 64 MB (padded), full YPIR setup takes ~3.5 seconds — well under the 75-second block interval. The database is padded to 8,192 rows from the start, so rebuild time is constant regardless of fill level.

### Server memory model

The server does not materialize the full tree. It stores only boundary nodes at depths 0, 16, 24, and 32:

- **Leaf commitments for the PIR window**: ~67 MB at capacity (32 shards × 65,536 leaves × 32 bytes)
- **Serialized PIR database**: 64 MB (fixed, padded)
- **Broadcast data**: < 1 MB (cap + sub-shard roots)
- **Total steady-state memory: ~131 MB**

What the server does **not** store:

- **Internal tree nodes** at depths 1–15, 17–23, and 25–31. The client reconstructs these from the boundary data (broadcast roots and PIR-returned leaves). Sub-shard root trees (depth 17–23) are recomputed from stored sub-shard roots on demand when building broadcast data.
- **Completed shards outside the PIR window**: folded to a 32-byte shard root and their leaf data discarded. The shard root survives permanently in the broadcast cap.
- During **initial sync**, only the current shard's leaves (~2 MB) are held in flight before being folded or stored.

### PIR window and eviction

The PIR database is a **fixed-size sliding window** over the most recent 32 shards. It does not start at shard 0 — it covers wherever the recent history is (e.g. shards 730–761). The broadcast metadata includes `window_start_shard` and `window_shard_count` so the client can compute physical row indices.

```
Before (window full, 32 shards):
  shard  730  731  732  ...  760  761
  ├──────────── PIR window ────────────┤
  window_start_shard = 730

New shard 762 completes:
  shard  731  732  733  ...  761  762
  ├──────────── PIR window ────────────┤
  window_start_shard = 731

Shard 730: leaf data discarded, 32-byte shard root stays in broadcast cap.
```

When a new shard completes and the window would exceed 32 shards, the oldest shard is evicted: `window_start_shard` increments, its 256 sub-shard rows are dropped from the PIR database, and its sub-shard roots are removed from the broadcast's active window section. Physical row indices shift — row 0 always maps to `window_start_shard`.

If a wallet's note falls outside the window, `get_witness()` returns `WitnessError::NoteOutsideWindow`. The wallet falls back to waiting for normal shard scanning. At current volume the window covers ~1.7 years, so this only affects very old notes.

### Pruning and reorg handling

**Server-side reorg**: When lightwalletd reports a chain reorganization, `commitment-tree-db.rollback_to(height)` removes all leaf commitments appended after `height`, recomputes the frontier shard's sub-shard roots, and triggers a PIR rebuild + `ArcSwap`. Completed shards are unaffected by reorgs — they are too deep to be rolled back.

**Wallet-side reorg**: When `truncate_to_height` is called during a reorg, `pir_witness_data` rows with `anchor_height > truncation_height` are deleted. The witness was obtained at a tree state that is no longer canonical. The wallet re-queries PIR after the reorg settles.

**Wallet-side note deletion**: `pir_witness_data` has `ON DELETE CASCADE` from `orchard_received_notes`. If the note itself is deleted, its witness is removed automatically.

**Wallet-side shard catch-up**: When the wallet finishes scanning a shard, the PIR witness for notes in that shard becomes redundant — `ShardTree` can now produce witnesses locally. The transaction builder tries `ShardTree` first and only falls back to `pir_witness_data` if the shard is incomplete. No cleanup is needed: PIR witnesses are a cache, not source of truth. They can be safely deleted at any time without data loss.

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
