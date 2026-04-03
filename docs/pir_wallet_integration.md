# PIR Wallet Integration Architecture

This document describes the end-to-end architecture of the PIR (Private Information Retrieval) nullifier spendability system as integrated into the Zcash iOS wallet. It covers the full path from on-chain nullifiers to wallet balance display.

## Problem Statement

When a Zcash wallet receives a shielded note, the note remains "unspent" in the wallet DB until the wallet scans the block containing the spending transaction. If the wallet is offline for hours or days, notes may have been spent on-chain but the wallet doesn't know yet. This creates two problems:

1. **Incorrect balance display:** The wallet shows a higher spendable balance than actually exists.
2. **Failed transactions:** The user constructs a transaction using a spent note, which is rejected at broadcast.

Traditional scanning resolves this eventually, but can take minutes. PIR provides an instant, private check.

## System Overview

The system spans four repositories and three runtime environments:

```
┌─────────────────────────────────────────────────────────────────────────┐
│ Server-side (Linux/cloud)                                               │
│                                                                         │
│  lightwalletd ──gRPC──> nf-ingest ──> HashTableDb ──> YPIR Engine      │
│                                                          │              │
│                                              spend-server (HTTP)        │
└────────────────────────────────────┬────────────────────────────────────┘
                                     │ HTTP (POST /query)
┌────────────────────────────────────┼────────────────────────────────────┐
│ Wallet Rust layer (iOS, compiled via XCFramework)                       │
│                                    │                                    │
│  spend-client ◄───────────────────┘                                    │
│       │                                                                 │
│  spendability.rs (FFI)                                                  │
│       │  ┌──────────────────────────────────────────┐                   │
│       ├──┤  get_unspent_orchard_notes_for_pir (read)  │                  │
│       │  └──────────────────────────────────────────┘                   │
│       │  ┌──────────────────────────────────────────┐                   │
│       ├──┤  insert_pir_spent_note (write-back)       │                  │
│       │  └──────────────────────────────────────────┘                   │
│       │  ┌──────────────────────────────────────────┐                   │
│       └──┤  get_pir_pending_spends (reconciliation)  │                  │
│          └──────────────┬───────────────────────────┘                   │
│                         │                                               │
│  zcash_client_sqlite    │                                               │
│       │                 ▼                                               │
│       │  ┌──────────────────────────────────────────┐                   │
│       ├──┤  pir_spent_notes table                    │                  │
│       │  └──────────────────────────────────────────┘                   │
│       │  ┌──────────────────────────────────────────┐                   │
│       └──┤  spent_notes_clause (UNION)               │                  │
│          └──────────────┬───────────────────────────┘                   │
│                         │                                               │
│                    Affects: get_wallet_summary                          │
│                            select_spendable_notes                      │
│                            select_unspent_notes                        │
│                            get_spendable_note                          │
│                            select_unspent_note_meta                    │
│                            unspent_notes_meta                          │
└────────────────────────────────────┬────────────────────────────────────┘
                                     │ Swift FFI
┌────────────────────────────────────┼────────────────────────────────────┐
│ Swift layer (iOS app)              │                                    │
│                                    │                                    │
│  zcash-swift-wallet-sdk            │                                    │
│       │                            │                                    │
│       ├── ZcashRustBackend ◄───────┘                                   │
│       └── SDKSynchronizer                                               │
│              │                                                          │
│  zodl-ios    │                                                          │
│       ├── RootInitialization (PIR trigger)                              │
│       ├── RootTransactions (PIR placeholder reconciliation)            │
│       ├── WalletBalancesStore (balance display)                        │
│       └── PIRDebugStore (diagnostic UI)                                │
└─────────────────────────────────────────────────────────────────────────┘
```

## Repository Map

| Repository | Role | PIR-relevant code |
|---|---|---|
| `sync-nullifier-pir` | PIR server + client library | `spend-server/`, `spend-client/`, `spend-types/`, `hashtable-pir/`, `nf-ingest/` |
| `zcash-swift-wallet-sdk` | Rust FFI layer + Swift SDK for iOS | `rust/src/spendability.rs` (FFI functions, DB queries, PIR client calls, pending-spend reconciliation), `SpendabilityBackend.swift` (Swift-side FFI bridge to `zcashlc_*` functions), `SDKSynchronizer.swift` (PIR entry point, sets `pirCompleted`), `ZcashRustBackend.swift` (Orchard spendability preserved when `pirCompleted`), `SDKFlags.swift` (`pirCompleted` flag lifecycle), `SpendabilityTypes.swift` (`PIRPendingSpends` / `PIRPendingNote` types) |
| `zcash_client_sqlite` | Wallet database crate (forked from upstream) | `src/wallet/common.rs` (`spent_notes_clause`, `unscanned_tip_exists` bypass), `src/wallet/db.rs` (table constants), `src/wallet/init/migrations/` (schema), `src/wallet.rs` (`truncate_to_height`, `get_wallet_summary`, `is_any_spendable` bypass) |
| `zodl-ios` | iOS app (TCA architecture) | `RootInitialization.swift` (PIR trigger), `RootTransactions.swift` (PIR placeholder reconciliation), `WalletBalancesStore.swift` (balance display), `PIRDebugStore.swift` (diagnostics) |

## Data Flow

### 1. Server-Side: Nullifier Ingestion

`nf-ingest` connects to `lightwalletd` via gRPC, parses compact blocks, extracts Orchard nullifiers, and feeds them as `ChainEvent`s into `HashTableDb` (a bucketed hash table). The hash table maps each 32-byte nullifier to a bucket via `hash_to_bucket(nf)` (first 4 bytes mod 16,384). The server handles reorgs by rolling back orphaned blocks.

`spend-server` rebuilds the YPIR database from the hash table after each chain event — new block or reorg — (~3s per rebuild) and swaps it in atomically via `ArcSwap`. Clients query via `POST /query` with an encrypted YPIR query for a single bucket.

### 2. Client-Side: PIR Query

`spend-client` provides `SpendClientBlocking`, a synchronous wrapper around `SpendClient`. For each nullifier:

1. Compute the bucket index via `hash_to_bucket(nf)`
2. Generate a YPIR SimplePIR query for that bucket
3. POST the query to the server (~672 KB upload)
4. Decode the encrypted response (~12 KB download)
5. Scan the decoded bucket for a nullifier match

The server learns which bucket was queried but not which entry within it, preserving privacy.

### 3. FFI Layer: `zcashlc_check_wallet_spendability`

This C FFI function in `spendability.rs` is the bridge between the wallet DB and the PIR client:

1. **Open** the wallet SQLite DB (read-write)
2. **Read** unspent Orchard nullifiers via `get_unspent_orchard_notes_for_pir` (excludes notes already in `orchard_received_note_spends` or `pir_spent_notes`)
3. **Connect** to the PIR server via `SpendClientBlocking::connect`
4. **Check** each nullifier via `check_nullifiers` (reports progress via callback)
5. **Write back** spent results into `pir_spent_notes` via `insert_pir_spent_note` (atomic conditional insert with retry)
6. **Return** `SpendabilityCheckResult` JSON (spent note IDs, total value, height range)

### 4. Swift Layer: Trigger and Display

**Trigger** (`RootInitialization.swift`):
- On app startup (foreground only, skipped for background tasks), fires `checkSpendabilityPIR`
- During sync, refires PIR on `.foundTransactions` and `.syncReachedUpToDate` events (debounced 5s)
- Stores the diagnostic result in `pirSpendabilityResult` shared state

**Spendability flag** (`SDKSynchronizer.swift` / `SDKFlags.swift`):
- After `checkWalletSpendability` succeeds, sets `sdkFlags.pirCompleted = true`
- This flag tells the Swift balance layer to preserve Orchard `spendableValue` even before `chainTipUpdated` is set (see Spendability Gates below)

**Balance** (`WalletBalancesStore.swift`):
- Queries `getAccountsBalances` which calls `get_wallet_summary`
- `get_wallet_summary` uses `spent_notes_clause` which UNIONs `pir_spent_notes`
- PIR-marked notes are excluded from both spendable and total shielded balance
- No overlay logic — the DB is the single source of truth

**Diagnostics** (`PIRDebugStore.swift`):
- Reads `pirSpendabilityResult` for display (spent note IDs, height range)
- Purely informational — does not affect balance logic

### 5. Transaction List: Note-Aware PIR Placeholders

When PIR detects spent notes, the wallet shows a synthetic "detected spend" entry in the transaction list so the user knows funds have moved before scanning confirms it. This placeholder must disappear as soon as scanning catches up — without waiting for a PIR re-check.

**The problem with caching:** A naive approach would cache the `SpendabilityResult` from the last PIR check and use it to build the placeholder. But PIR results go stale: scanning may confirm note A's spend in the wallet DB while the cached result still lists note A as pending. This creates a window where the user sees both the real transaction and the stale placeholder.

**DB-backed reconciliation:** Instead of using the cached PIR result, the transaction list queries the wallet DB directly via `zcashlc_get_pir_pending_spends`. This FFI function runs a read-only query that returns only PIR-detected spends not yet confirmed by scanning:

```sql
SELECT pir.note_id, rn.value
FROM pir_spent_notes pir
JOIN orchard_received_notes rn ON pir.note_id = rn.id
WHERE NOT EXISTS (
    SELECT 1 FROM orchard_received_note_spends sp
    WHERE sp.orchard_received_note_id = pir.note_id
)
```

The result (`PIRPendingSpends`) contains per-note IDs and values, giving the placeholder note-level granularity. As scanning confirms each note's spend (inserts into `orchard_received_note_spends`), that note drops out of the query result automatically.

**Integration in `RootTransactions.swift`:**

```
fetchTransactionsForTheSelectedAccount
    │
    ├── async let txTask = getAllTransactions(accountUUID)
    │
    └── async let pirTask = getPIRPendingSpends()
                │
                ▼
        fetchedTransactions(transactions, pirPendingSpends)
                │
                ├── Swap resolution (unchanged)
                │
                ├── if pirPending.notes is non-empty:
                │       append TransactionState(
                │           pirDetectedSpentValue: totalValue,
                │           noteIds: [noteId...]
                │       )
                │
                └── Sort and display
```

The `pirNoteIds` field on `TransactionState` tracks which wallet note IDs the placeholder represents. This makes the placeholder note-aware: each refresh queries the DB for the current set of unreconciled notes, so the placeholder's value shrinks as scanning catches up, and disappears entirely when all notes are confirmed.

**Timing:** The two async calls (`getAllTransactions` and `getPIRPendingSpends`) run concurrently. The pending-spends query is read-only and lightweight (no network I/O, just a SQLite JOIN). The debounced PIR re-check in `foundTransactions` still runs to detect *new* spends; the `getPIRPendingSpends` query handles reconciliation of *existing* detections.

## Database Schema

### `pir_spent_notes` Table

```sql
CREATE TABLE pir_spent_notes (
    note_id INTEGER NOT NULL PRIMARY KEY
        REFERENCES orchard_received_notes(id) ON DELETE CASCADE
)
```

- **Single column:** `note_id` references `orchard_received_notes.id`
- **Orchard-only:** PIR only applies to Orchard shielded notes
- **FK cascade:** Rows are automatically deleted when the parent note is deleted (account deletion, full DB wipe)
- **No transaction reference:** Unlike `orchard_received_note_spends`, there is no associated spending transaction — PIR only confirms a nullifier was spent, not which transaction spent it

### Integration Point: `spent_notes_clause`

```
spent_notes_clause("orchard")
    │
    ├── SELECT orchard_received_note_id FROM orchard_received_note_spends
    │   WHERE tx_unexpired_condition(...)
    │
    └── UNION SELECT note_id FROM pir_spent_notes    ← added by PIR
```

This function is used by all balance and note-selection queries. The UNION is compiled in only when the `sync-nullifier-pir` Cargo feature is enabled.

### Lifecycle of a PIR Row

```
Note received ──> PIR check ──> INSERT into pir_spent_notes
                                        │
                    ┌───────────────────┤
                    │                   │
                    ▼                   ▼
            Scanning finds         No further action
            real spend             (row persists, harmless)
                    │
                    ▼
            Row in orchard_received_note_spends
            (pir row redundant, UNION deduplicates)

Cleared by:
  • truncate_to_height (reorg/rescan)  ──> DELETE FROM pir_spent_notes
  • Account deletion                    ──> FK ON DELETE CASCADE
```

## Spendability Gates

Without PIR, the SDK uses two independent safety mechanisms to prevent spending notes whose nullifiers may have been published in blocks the wallet hasn't scanned yet. Both force `orchardBalance.spendableValue` to zero during sync, which blocks the Send form (`isInsufficientFunds` checks `amount > shieldedBalance` where `shieldedBalance` derives from `spendableValue`). A third gate operates at the Swift layer before the value reaches the UI.

PIR provides the same safety guarantee these gates enforce — it confirms whether each note's nullifier is on-chain — making them redundant for Orchard when the feature is enabled.

### Gate 1: `is_any_spendable` (Rust, `wallet.rs`)

In `get_wallet_summary`, the `is_any_spendable` function checks whether any unscanned shard ranges overlap the anchor height:

```sql
SELECT NOT EXISTS(
    SELECT 1 FROM v_orchard_shard_unscanned_ranges
    WHERE :anchor_height BETWEEN subtree_start_height
        AND IFNULL(subtree_end_height, :anchor_height)
    AND block_range_start <= :anchor_height
)
```

If unscanned ranges exist, `any_spendable = false` and every note's value is routed to `value_pending_spendability` instead of `spendable_value`. The total balance is unchanged, but the spendable portion is zero.

**PIR bypass:** When `sync-nullifier-pir` is enabled and `table_prefix == "orchard"`, `any_spendable` is unconditionally `true`. Sapling retains the original check.

### Gate 2: `unscanned_tip_exists` (Rust, `common.rs`)

In `select_spendable_notes_matching_value`, the same SQL query (inverted) guards note selection for `proposeTransfer`. If unscanned ranges overlap the anchor height, the function returns an empty vec — even if Gate 1 has been bypassed and the UI shows a spendable balance, the transaction proposal would fail here.

**PIR bypass:** When `sync-nullifier-pir` is enabled and `protocol == ShieldedProtocol::Orchard`, the `unscanned_tip_exists` check is skipped. Sapling retains the original check.

### Gate 3: `chainTipUpdated` (Swift, `ZcashRustBackend.swift`)

After `getWalletSummary()` returns from Rust, the Swift SDK checks `sdkFlags.chainTipUpdated`. If false (app backgrounded > 120s and `UpdateChainTipAction` hasn't run yet), it overwrites `spendableValue` to zero for both pools and moves the amounts into `valuePendingSpendability`. This is independent of the Rust gates.

The flag lifecycle (in `SDKFlags.swift`):
- `sdkStopped()` → `chainTipUpdated = false`
- `sdkStarted()` → restores to `true` only if stopped < 120 seconds ago
- `UpdateChainTipAction` → calls `markChainTipAsUpdated()`

**PIR bypass:** A separate `pirCompleted` flag (same lifecycle as `chainTipUpdated`) is set after `checkWalletSpendability` succeeds. When `chainTipUpdated` is false but `pirCompleted` is true, Orchard's `spendableValue` is preserved. Sapling is still zeroed.

### Combined effect

```
                              Feature OFF              Feature ON
                              ───────────              ──────────
get_wallet_summary            is_any_spendable         Orchard: always true
  (balance display)           gates all pools          Sapling: unchanged

select_spendable_notes        unscanned_tip_exists     Orchard: check skipped
  (proposeTransfer)           returns empty vec         Sapling: unchanged

getWalletSummary (Swift)      chainTipUpdated gates    Orchard: preserved if
                              both pools               pirCompleted is true
                                                       Sapling: unchanged
```

### Safety with PIR enabled

- `spent_notes_clause` already excludes PIR-marked spent notes from all balance and selection queries.
- Notes NOT in `pir_spent_notes` have been confirmed unspent by PIR's on-chain nullifier check.
- If PIR hasn't run (server unreachable), `pir_spent_notes` is empty. The user could attempt to spend a note that was actually spent in unscanned blocks; the transaction would fail at broadcast (nullifier already on-chain). No funds are lost.
- Newly discovered notes (found after PIR's last run) are not yet validated. The `foundTransactions` / `syncReachedUpToDate` handler triggers a debounced PIR re-check within 5 seconds, limiting the exposure window.

## Feature Flag Strategy

The `sync-nullifier-pir` Cargo feature in `zcash_client_sqlite` controls PIR integration:

| Aspect | Feature OFF | Feature ON |
|---|---|---|
| `pir_spent_notes` table | Exists (migration unconditional) | Exists |
| Table contents | Always empty | Populated by PIR |
| `spent_notes_clause` | Original query (no UNION) | UNION with `pir_spent_notes` |
| `is_any_spendable` (Orchard) | Checked normally | Bypassed (always true) |
| `unscanned_tip_exists` (Orchard) | Checked normally | Bypassed (skipped) |
| `truncate_to_height` | DELETE is a no-op (empty table) | Clears PIR rows |
| Crate behavior | Identical to upstream | Integrates PIR exclusions + spendability bypass |

The feature is enabled in `zcash-swift-wallet-sdk/Cargo.toml` and disabled by default in `zcash_client_sqlite/Cargo.toml`.

The Swift-level `pirCompleted` flag in `SDKFlags` is always compiled in but has no effect unless the Rust layer is built with `sync-nullifier-pir` (the `checkWalletSpendability` FFI function only exists when the feature is enabled).

## Concurrency Model

Two writers access the wallet SQLite DB:

| Writer | Connection | Writes to |
|---|---|---|
| SDK sync loop | Managed by `@DBActor` | `orchard_received_notes`, `orchard_received_note_spends`, `transactions`, etc. |
| PIR FFI call | Opened directly in `zcashlc_check_wallet_spendability` | `pir_spent_notes` only |

SQLite (even in WAL mode) allows only one writer at a time. The PIR connection uses an explicit retry loop with exponential backoff (50ms–6.4s, 8 retries) to handle `SQLITE_BUSY`. The two writers target separate tables, so there is no row-level conflict — only write-lock contention.

### Race Condition Prevention

PIR and scanning can operate on the same note concurrently. Two layers prevent inconsistency:

1. **Read-time exclusion:** `get_unspent_orchard_notes_for_pir` excludes notes already in `orchard_received_note_spends` or `pir_spent_notes`. If scanning marks a note before PIR reads, PIR never sees it.

2. **Atomic conditional insert:** The PIR INSERT uses `NOT EXISTS` guards against both `orchard_received_note_spends` and `pir_spent_notes`. SQLite's write serialization ensures these checks and the INSERT execute atomically.

## Cross-Crate Dependency Graph

```
sync-nullifier-pir/spend-client
        │
        │ (linked into libzcashlc via Cargo)
        ▼
zcash-swift-wallet-sdk/rust (spendability.rs)
        │
        │ (path dependency with sync-nullifier-pir feature)
        ▼
zcash_client_sqlite (spent_notes_clause, pir_spent_notes table)
        │
        │ (Swift FFI via XCFramework)
        ▼
zcash-swift-wallet-sdk (Swift SDK)
        │
        │ (SPM dependency)
        ▼
zodl-ios (iOS app)
```

The PIR server (`spend-server`, `nf-ingest`, `hashtable-pir`) runs independently and communicates with the wallet solely via HTTP.
