# PIR Wallet Integration Architecture

This document describes the wallet-side integration of two PIR (Private Information Retrieval) subsystems for the Zcash iOS wallet: **nullifier PIR** (spend detection) and **witness PIR** (immediate spendability). It covers FFI contracts, database schema, feature flags, Swift-side orchestration, and behavioral effects on balance and spending.

For server-side architecture (tree decomposition, YPIR parameters, ingestion pipelines), see [note-witness/README.md](../note-witness/README.md).

## Problem Statement

During wallet sync, two blockers prevent a smooth user experience:

1. **Stale balance (nullifier PIR):** When a note has been spent on-chain but the wallet hasn't scanned the spending block yet, the wallet shows a higher spendable balance than actually exists. Attempting to spend such a note fails at broadcast. Nullifier PIR privately checks whether each note's nullifier is on-chain and excludes spent notes from balance immediately.

2. **Delayed spendability (witness PIR):** When the wallet discovers a new note during sync, it cannot spend it until the local ShardTree shard is fully scanned — typically 30s to several minutes. Witness PIR fetches the Merkle authentication path from a server, enabling the note to be spent immediately.

## System Overview

The system spans four repositories and three runtime environments:

```
┌─────────────────────────────────────────────────────────────────────────┐
│ Server-side (Linux/cloud)                                               │
│                                                                         │
│  spend-server (HTTP)              witness-server (HTTP)                 │
│  POST /query — nullifier check    POST /query — witness path            │
│                                   GET /broadcast — tree metadata        │
└──────────────┬──────────────────────────────┬──────────────────────────┘
               │                              │
┌──────────────┼──────────────────────────────┼──────────────────────────┐
│ Wallet Rust layer (iOS, compiled via XCFramework)                       │
│              │                              │                           │
│  spend-client ◄────────────────┘  witness-client ◄─────────┘           │
│       │                                │                                │
│  spendability.rs (FFI)            witness.rs (FFI)                      │
│       │                                │                                │
│  zcash_client_sqlite                   │                                │
│       │                                │                                │
│       │  ┌──────────────────────────────────────┐                       │
│       ├──┤  pir_spent_notes table                │                      │
│       │  └──────────────────────────────────────┘                       │
│       │  ┌──────────────────────────────────────┐                       │
│       ├──┤  spent_notes_clause (UNION)           │                      │
│       │  └──────────────────────────────────────┘                       │
│       │  ┌──────────────────────────────────────┐                       │
│       ├──┤  pir_witness_data table               │                      │
│       │  └──────────────────────────────────────┘                       │
│       │  ┌──────────────────────────────────────┐                       │
│       └──┤  shard_scanned_condition bypass       │                      │
│          └──────────────────────────────────────┘                       │
│                                                                         │
│  zcash_client_backend                                                   │
│       └── pir_orchard_witness_fallback (tx builder)                     │
│                                                                         │
│  Affects: get_wallet_summary, select_spendable_notes,                   │
│           create_proposed_transactions                                  │
└────────────────────────────────────┬────────────────────────────────────┘
                                     │ Swift FFI
┌────────────────────────────────────┼────────────────────────────────────┐
│ Swift layer (iOS app)              │                                    │
│                                    │                                    │
│  zcash-swift-wallet-sdk            │                                    │
│       ├── SpendabilityBackend ◄────┘                                   │
│       ├── WitnessBackend ◄─────────┘                                   │
│       └── SDKSynchronizer                                               │
│              │                                                          │
│  zodl-ios    │                                                          │
│       ├── RootInitialization (PIR triggers)                             │
│       ├── RootTransactions (PIR placeholder reconciliation)            │
│       ├── WalletBalancesStore (balance display)                        │
│       └── PIRDebugStore (diagnostic UI)                                │
└─────────────────────────────────────────────────────────────────────────┘
```

## Repository Map

| Repository | Role | Nullifier PIR code | Witness PIR code |
|---|---|---|---|
| `spendability-pir` | PIR servers + client libraries | `nullifier/spend-client/`, `nullifier/spend-types/` | `witness/witness-client/`, `witness/witness-types/` |
| `zcash-swift-wallet-sdk` | Rust FFI + Swift SDK | `rust/src/spendability.rs`, `SpendabilityBackend.swift`, `SpendabilityTypes.swift` | `rust/src/witness.rs`, `WitnessBackend.swift`, `WitnessTypes.swift` |
| `zcash_client_sqlite` | Wallet database crate | `src/wallet/common.rs` (`spent_notes_clause`, `unscanned_tip_exists` bypass), `src/wallet.rs` (`is_any_spendable` bypass, `truncate_to_height`) | `src/wallet/pir_witness.rs`, `src/wallet/common.rs` (`shard_scanned_condition` bypass) |
| `zcash_client_backend` | Wallet logic crate | — | `src/data_api.rs` (`get_pir_orchard_merkle_path` trait method), `src/data_api/wallet.rs` (`pir_orchard_witness_fallback`) |
| `zodl-ios` | iOS app (TCA) | `RootInitialization.swift` (`.checkSpendabilityPIR`), `RootTransactions.swift` (placeholders), `PIRDebugStore.swift` | `RootInitialization.swift` (`.checkWitnessPIR`), `PIRDebugStore.swift` (witness section) |

Both SDKSynchronizer orchestration methods (`checkWalletSpendability`, `fetchNoteWitnesses`) and the shared `SDKFlags.swift` (`pirCompleted` flag lifecycle) span both subsystems.

## Data Flow

### Nullifier PIR

#### Server Contract

The spend-server exposes a YPIR-based nullifier lookup over HTTP. The wallet queries whether specific nullifiers appear on-chain; the server learns which bucket was queried but not which entry, preserving privacy.

#### FFI Layer

The nullifier PIR flow is split across multiple FFI functions to keep network I/O separate from DB access:

**`zcashlc_check_nullifiers_pir`** (in `spendability.rs`) — network-only call:
- Input: JSON array of nullifiers (hex-encoded 32-byte values), server URL, progress callback
- Output: JSON `NullifierCheckResult { earliest_height, latest_height, spent: [bool] }`
- Connects to the spend-server via `SpendClientBlocking::connect`, checks all nullifiers via `check_nullifiers`

**DB helpers** (in `lib.rs`, accessed through `@DBActor`):
- `zcashlc_get_unspent_orchard_notes_for_pir` — reads unspent Orchard notes (excludes notes already in `orchard_received_note_spends` or `pir_spent_notes`)
- `zcashlc_insert_pir_spent_notes` — writes spent results into `pir_spent_notes` (atomic conditional insert)
- `zcashlc_get_pir_pending_spends` — reads PIR-detected spends not yet confirmed by scanning (for transaction list placeholders)

#### Swift Orchestration

`SDKSynchronizer.checkWalletSpendability(pirServerUrl, progress)`:
1. Read unspent notes via `getUnspentOrchardNotesForPIR()`
2. Call `SpendabilityBackend().checkNullifiersPIR()` (network, runs on detached task)
3. Write back spent results via `insertPIRSpentNotes()`
4. Set `sdkFlags.pirCompleted = true`
5. Return `SpendabilityResult` (spent note IDs, total value, height range)

#### App Trigger

In `RootInitialization.swift`:
- On app startup (foreground only), fires `.checkSpendabilityPIR`
- During sync, refires on `.foundTransactions` and `.syncReachedUpToDate` events (debounced 5s)
- Gated on `walletConfig.isEnabled(.pirSpendability)`

#### Transaction List: Note-Aware Placeholders

When PIR detects spent notes, the wallet shows a synthetic "detected spend" entry in the transaction list. This placeholder uses DB-backed reconciliation rather than cached PIR results to avoid stale entries.

`zcashlc_get_pir_pending_spends` returns only PIR-detected spends not yet confirmed by scanning:

```sql
SELECT pir.note_id, rn.value
FROM pir_spent_notes pir
JOIN orchard_received_notes rn ON pir.note_id = rn.id
WHERE NOT EXISTS (
    SELECT 1 FROM orchard_received_note_spends sp
    WHERE sp.orchard_received_note_id = pir.note_id
)
```

In `RootTransactions.swift`, `getAllTransactions` and `getPIRPendingSpends` run concurrently. If pending notes exist, a `TransactionState` placeholder is appended. The placeholder's value shrinks as scanning confirms each note's spend and disappears when all are reconciled.

### Witness PIR

#### Server Contract

The witness-server provides Merkle authentication paths for Orchard notes at a fixed anchor height (`tip - 10`). This anchor satisfies all wallet confirmation policies (trusted: 3, untrusted: 10, shielding: 1). See [note-witness/README.md](../note-witness/README.md) for server architecture details.

#### FFI Layer

Like nullifier PIR, network I/O and DB access are separated:

**`zcashlc_fetch_pir_witnesses`** (in `witness.rs`) — network-only call:
- Input: JSON array of `{ "note_id": i64, "position": u64 }`, server URL, progress callback
- Output: JSON `WitnessCheckResult { witnesses: [WitnessEntry] }`

Each `WitnessEntry` contains:
- `note_id`, `position` — identifiers
- `siblings` — 32 hex-encoded 32-byte sibling hashes (leaf-to-root)
- `anchor_height` — chain height the witness is anchored to
- `anchor_root` — hex-encoded tree root for self-verification

Queries are issued **per-note** via `WitnessClientBlocking::get_witness`. If a query fails (e.g. note position outside the server's window), the error is logged with `tracing::warn!` and that note is skipped. Partial results are returned — see [Error Handling](#error-handling) below.

**DB helpers** (in `lib.rs`, accessed through `@DBActor`):
- `zcashlc_get_notes_needing_pir_witness` — reads notes that have a tree position, are unspent, and lack a PIR witness
- `zcashlc_insert_pir_witnesses` — stores witness data into `pir_witness_data`
- `zcashlc_get_pir_witnessed_notes` — lists notes with PIR witnesses (for diagnostics)

#### Swift Orchestration

`SDKSynchronizer.fetchNoteWitnesses(pirServerUrl, progress)`:
1. Read notes needing witnesses via `getNotesNeedingPIRWitness()`
2. Call `WitnessBackend().fetchWitnesses()` (network, runs on detached task)
3. Store results via `insertPIRWitnesses()`
4. Return `WitnessResult` (witnessed note IDs, total value)

#### App Trigger

In `RootInitialization.swift`:
- Fired alongside `checkSpendabilityPIR` on app startup and sync events
- Gated on `walletConfig.isEnabled(.pirWitness)`

#### Transaction Builder Fallback

When the Orchard ShardTree cannot produce a witness (shard incomplete, checkpoint missing), the transaction builder in `zcash_client_backend` automatically falls back to PIR-stored witnesses. This requires no wallet-side code changes beyond enabling the `sync-witness-pir` feature flag.

In `build_proposed_transaction`:
1. Attempt `witness_at_checkpoint_id_caching` from the ShardTree
2. If that errors, call `pir_orchard_witness_fallback`
3. Fallback reads `pir_witness_data` via `get_pir_orchard_merkle_path(position)` for each Orchard note
4. All PIR witnesses must share the same anchor root — that root becomes the transaction's Orchard anchor
5. If any note lacks a PIR witness, the fallback fails (the note cannot be spent yet)

## Database Schema

### `pir_spent_notes` Table (Nullifier PIR)

```sql
CREATE TABLE pir_spent_notes (
    note_id INTEGER NOT NULL PRIMARY KEY
        REFERENCES orchard_received_notes(id) ON DELETE CASCADE
)
```

- **Single column:** `note_id` references `orchard_received_notes.id`
- **No transaction reference:** PIR confirms a nullifier was spent, not which transaction spent it

**Integration point — `spent_notes_clause`:** When `spendability-pir` is enabled and the table prefix is `"orchard"`, the clause UNIONs `pir_spent_notes` into the spent-notes subquery. This affects all balance and note-selection queries (`get_wallet_summary`, `select_spendable_notes`, etc.).

**Lifecycle:**

```
Note received ──> PIR check ──> INSERT into pir_spent_notes
                                        │
                    ┌───────────────────┤
                    ▼                   ▼
            Scanning finds         No further action
            real spend             (row persists, UNION deduplicates)

Cleared by:
  • truncate_to_height (reorg/rescan)  ──> DELETE FROM pir_spent_notes
  • Account deletion                    ──> FK ON DELETE CASCADE
```

### `pir_witness_data` Table (Witness PIR)

```sql
CREATE TABLE pir_witness_data (
    note_id INTEGER NOT NULL PRIMARY KEY
        REFERENCES orchard_received_notes(id) ON DELETE CASCADE,
    siblings BLOB NOT NULL CHECK(length(siblings) = 1024),
    anchor_height INTEGER NOT NULL,
    anchor_root BLOB NOT NULL CHECK(length(anchor_root) = 32)
)
```

- `siblings`: 32 x 32-byte hashes = 1024 bytes — the full Merkle authentication path (leaf-to-root)
- `anchor_height`: the chain height the witness is anchored to (server's `tip - CONFIRMATION_DEPTH`)
- `anchor_root`: the tree root for self-verification and anchor construction

**Integration point — `shard_scanned_condition`:** When `sync-witness-pir` is enabled for Orchard, coin selection accepts notes that have a PIR witness even if their shard is not fully scanned:

```
shard_scanned_condition(Orchard)
    │
    ├── scan_state.max_priority <= :scanned_priority    (original check)
    │
    └── OR EXISTS (SELECT 1 FROM pir_witness_data       (witness PIR bypass)
                   WHERE note_id = rn.id)
```

**Lifecycle:**

```
Note discovered ──> PIR query ──> INSERT into pir_witness_data
        │                                  │
        │                    ┌─────────────┤
        │                    ▼             ▼
        │             Tx builder       Wallet fully
        │             reads witness    syncs (ShardTree
        │             as fallback      can produce witness
        │                              locally — PIR row
        │                              persists, harmless)
        │
Cleared by:
  • truncate_to_height (reorg/rescan)  ──> DELETE FROM pir_witness_data
  • Account deletion                    ──> FK ON DELETE CASCADE
```

Both tables are created unconditionally by their migrations (schema is identical across all builds). When the corresponding feature is off, the tables are empty and unused.

## Spendability Gates

Without PIR, four independent safety mechanisms prevent spending notes before the wallet is confident they are unspent and can construct a valid proof. PIR bypasses these for Orchard when enabled.

### Gate 1: `is_any_spendable` (Rust, `wallet.rs`, `spendability-pir`)

In `get_wallet_summary`, checks whether any unscanned shard ranges overlap the anchor height. If they do, all notes are routed to `value_pending_spendability` — the balance display shows zero spendable.

**PIR bypass:** When `spendability-pir` is enabled, `any_spendable` is unconditionally `true` for Orchard. Sapling retains the original check.

### Gate 2: `unscanned_tip_exists` (Rust, `common.rs`, `spendability-pir`)

In `select_spendable_notes`, returns an empty vec if unscanned ranges exist — `proposeTransfer` fails even if the UI shows a balance.

**PIR bypass:** When `spendability-pir` is enabled, the check is skipped for Orchard.

### Gate 3: `chainTipUpdated` (Swift, `ZcashRustBackend.swift`)

After `getWalletSummary()` returns, the Swift SDK checks `sdkFlags.chainTipUpdated`. If false (app backgrounded > 120s), `spendableValue` is zeroed for both pools.

**PIR bypass:** A separate `pirCompleted` flag is set after `checkWalletSpendability` succeeds. When `chainTipUpdated` is false but `pirCompleted` is true, Orchard's `spendableValue` is preserved.

### Gate 4: `shard_scanned_condition` (Rust, `common.rs`, `sync-witness-pir`)

In `select_spendable_notes`, requires each note's shard to be fully scanned before it can be selected for spending.

**PIR bypass:** When `sync-witness-pir` is enabled, notes with a row in `pir_witness_data` bypass this check for Orchard — a PIR witness provides the authentication path that the incomplete shard cannot.

### Combined Effect

```
                              Features OFF             Features ON
                              ────────────             ───────────
get_wallet_summary            is_any_spendable         Orchard: always true
  (balance display)           gates all pools          Sapling: unchanged

select_spendable_notes        unscanned_tip_exists     Orchard: check skipped
  (proposeTransfer)           returns empty vec         Sapling: unchanged

select_spendable_notes        shard_scanned_condition  Orchard: PIR-witnessed
  (coin selection)            requires full shard      notes accepted
                                                       Sapling: unchanged

getWalletSummary (Swift)      chainTipUpdated gates    Orchard: preserved if
                              both pools               pirCompleted is true
                                                       Sapling: unchanged
```

### Safety with PIR Enabled

- `spent_notes_clause` excludes PIR-marked spent notes from all balance and selection queries.
- Notes NOT in `pir_spent_notes` have been confirmed unspent by PIR's on-chain nullifier check.
- Notes with PIR witnesses can construct valid spend proofs using the server-provided authentication path, which is self-verified against the anchor root.
- If PIR servers are unreachable, `pir_spent_notes` and `pir_witness_data` are empty. The wallet falls back to standard scanning behavior — spendability is delayed but never blocked. No funds are lost.
- Newly discovered notes trigger a debounced PIR re-check within 5 seconds via `foundTransactions` / `syncReachedUpToDate`.

## Feature Flag Strategy

Two Cargo features in `zcash_client_sqlite` control PIR integration:

### `spendability-pir`

| Aspect | Feature OFF | Feature ON |
|---|---|---|
| `pir_spent_notes` table | Exists (migration unconditional) | Exists |
| Table contents | Always empty | Populated by PIR |
| `spent_notes_clause` | Original query (no UNION) | UNION with `pir_spent_notes` |
| `is_any_spendable` (Orchard) | Checked normally | Bypassed (always true) |
| `unscanned_tip_exists` (Orchard) | Checked normally | Bypassed (skipped) |
| `truncate_to_height` | DELETE is a no-op (empty table) | Clears PIR rows |

### `sync-witness-pir`

| Aspect | Feature OFF | Feature ON |
|---|---|---|
| `pir_witness_data` table | Exists (migration unconditional) | Exists |
| `pir_witness` module | Not compiled | Compiled |
| `shard_scanned_condition` (Orchard) | Original check | Accepts PIR-witnessed notes |
| Transaction builder | ShardTree only | Falls back to PIR witnesses |
| `truncate_to_height` | DELETE is a no-op (empty table) | Clears PIR witness rows |

Both features are enabled together in `zcash-swift-wallet-sdk/Cargo.toml` and disabled by default in `zcash_client_sqlite/Cargo.toml`. The Swift-level `pirCompleted` flag in `SDKFlags` is always compiled in but has no effect unless the Rust layer is built with the features enabled.

## Concurrency Model

Three writers access the wallet SQLite DB:

| Writer | Connection | Writes to |
|---|---|---|
| SDK sync loop | Managed by `@DBActor` | `orchard_received_notes`, `orchard_received_note_spends`, `transactions`, etc. |
| Nullifier PIR DB helpers | Through `@DBActor` | `pir_spent_notes` only |
| Witness PIR DB helpers | Through `@DBActor` | `pir_witness_data` only |

The PIR network calls (`zcashlc_check_nullifiers_pir`, `zcashlc_fetch_pir_witnesses`) open no database connections — they are pure network I/O. DB reads and writes go through the `@DBActor`-managed connection via the `zcashlc_*` helpers in `lib.rs`.

SQLite (even in WAL mode) allows only one writer at a time. The PIR and sync writers target separate tables, so there is no row-level conflict — only write-lock contention, handled by SQLite's busy retry.

### Race Condition Prevention

PIR and scanning can operate on the same note concurrently. Two layers prevent inconsistency:

1. **Read-time exclusion:** `get_unspent_orchard_notes_for_pir` and `get_notes_needing_pir_witness` both exclude notes already in `orchard_received_note_spends`, `pir_spent_notes`, or `pir_witness_data`. If scanning processes a note before PIR reads, PIR skips it.

2. **Conditional insert:** Both PIR INSERT statements use `NOT EXISTS` guards. SQLite's write serialization ensures the check and INSERT execute atomically.

## Error Handling

The two PIR subsystems handle failures differently:

**Nullifier PIR** uses batch semantics: `SpendClientBlocking::check_nullifiers` processes all nullifiers in sequence. If the server is unreachable, no results are returned. The wallet degrades gracefully — it shows the balance as-is and the user can retry.

**Witness PIR** uses per-note semantics: `WitnessClientBlocking::get_witness` is called individually for each note. If a query fails (e.g. `PositionOutsideWindow` because the note's position is outside the server's active shard window), the error is logged with `tracing::warn!` and that note is skipped. Other notes in the batch can still succeed. This is important because notes at different positions may fall in or out of the server's window independently.

In both cases, server unavailability is non-fatal. The wallet falls back to standard scanning — spendability is delayed but never blocked.

## Cross-Crate Dependency Graph

```
spendability-pir/spend-client ──┐
                                   ├──> zcash-swift-wallet-sdk/rust (libzcashlc)
spendability-pir/witness-client ─┘           │
                                               │ path dependency
                                               ▼
                                        zcash_client_sqlite
                                          ├── pir_spent_notes + spent_notes_clause
                                          └── pir_witness_data + shard_scanned_condition
                                               │
                                               │ path dependency
                                               ▼
                                        zcash_client_backend
                                          └── pir_orchard_witness_fallback (tx builder)
                                               │
                                               │ Swift FFI via XCFramework
                                               ▼
                                        zcash-swift-wallet-sdk (Swift SDK)
                                               │
                                               │ SPM dependency
                                               ▼
                                        zodl-ios (iOS app)
```

The PIR servers (`spend-server`, `witness-server`) run independently and communicate with the wallet solely via HTTP.
