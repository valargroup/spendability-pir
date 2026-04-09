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
│  spend-client ◄─────────────────┘  witness-client ◄──────────┘         │
│       │                                │                                │
│  spendability.rs (network-only FFI)    witness.rs (network-only FFI)   │
│       │                                │                                │
│  change_discovery.rs (trial decrypt, compact block extraction)          │
│       │                                │                                │
│  lib.rs (DB-facing FFI: all zcashlc_* helpers)                          │
│       │                                                                 │
│       ├── zcashlc_get_unspent_orchard_notes_for_pir                    │
│       ├── zcashlc_insert_pir_spent_notes                               │
│       ├── zcashlc_discover_change_notes (decrypt + activity metadata)   │
│       ├── zcashlc_get_provisional_notes_for_pir                        │
│       ├── zcashlc_mark_provisional_pir_results                         │
│       ├── zcashlc_get_pir_activity_entries                             │
│       ├── zcashlc_get_notes_needing_pir_witness                        │
│       ├── zcashlc_get_provisional_notes_needing_witness                │
│       ├── zcashlc_insert_pir_witnesses (with root validation)          │
│       ├── zcashlc_mark_provisional_note_witnessed                      │
│       ├── zcashlc_get_pir_witnessed_notes                              │
│       └── zcashlc_get_pir_witness_notes_for_proposal                   │
│       │                                                                 │
│  zcash_client_sqlite                                                    │
│       │  ┌──────────────────────────────────────┐                       │
│       ├──┤  pir_notes table (unified lifecycle)  │                      │
│       │  └──────────────────────────────────────┘                       │
│       │  ┌──────────────────────────────────────┐                       │
│       ├──┤  spent_notes_clause (UNION)           │                      │
│       │  └──────────────────────────────────────┘                       │
│       │  ┌──────────────────────────────────────┐                       │
│       ├──┤  shard_scanned_condition bypass       │                      │
│       │  └──────────────────────────────────────┘                       │
│       │  ┌──────────────────────────────────────┐                       │
│       └──┤  provisional notes in coin selection  │                      │
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
│       ├── SDKSynchronizer (orchestration)                               │
│       │     ├── checkWalletSpendability (recursive)                    │
│       │     ├── fetchNoteWitnesses                                     │
│       │     └── getPIRActivityEntries                                  │
│       └── ZcashRustBackend (@DBActor)                                  │
│              │                                                          │
│  zodl-ios    │                                                          │
│       ├── RootInitialization (PIR triggers)                             │
│       ├── RootTransactions (activity merge with PIR entries)            │
│       ├── WalletBalancesStore (balance display)                        │
│       └── PIRDebugStore (diagnostic UI)                                │
└─────────────────────────────────────────────────────────────────────────┘
```

## Repository Map

| Repository | Role | Key PIR code |
|---|---|---|
| `spendability-pir` | PIR servers + client libraries | `nullifier/spend-client/`, `nullifier/spend-types/`, `witness/witness-client/`, `witness/witness-types/` |
| `zcash-swift-wallet-sdk` | Rust FFI + Swift SDK | **Rust:** `rust/src/spendability.rs` (network), `rust/src/witness.rs` (network), `rust/src/change_discovery.rs` (trial decrypt), `rust/src/lib.rs` (all DB-facing `zcashlc_*`). **Swift:** `SpendabilityBackend.swift`, `SpendabilityTypes.swift`, `WitnessBackend.swift`, `WitnessTypes.swift`, `SDKSynchronizer.swift` |
| `zcash_client_sqlite` | Wallet database crate | `src/wallet/pir.rs` (unified: spend tracking, witness storage, provisional lifecycle, activity entries, Merkle path construction, reconciliation), `src/wallet/common.rs` (`spent_notes_clause`, `shard_scanned_condition`, provisional notes in coin selection), `src/wallet.rs` (`is_any_spendable` bypass, provisional balance, `truncate_to_height`) |
| `zcash_client_backend` | Wallet logic crate | `src/data_api.rs` (`get_pir_orchard_merkle_path` trait method), `src/data_api/wallet.rs` (`pir_orchard_witness_fallback`) |
| `zodl-ios` | iOS app (TCA) | `RootInitialization.swift` (PIR triggers), `RootTransactions.swift` (activity merge), `PIRDebugStore.swift` (diagnostics) |

Both SDKSynchronizer orchestration methods (`checkWalletSpendability`, `fetchNoteWitnesses`) and the shared `SDKFlags.swift` (`pirCompleted` flag lifecycle) span both subsystems.

## Key Concepts: Canonical vs Provisional Notes

The PIR system distinguishes two kinds of notes based on their origin and relationship to the wallet's main `orchard_received_notes` table.

### Canonical Notes

A **canonical note** is one the wallet already knows about because the scanner discovered it during normal sync. It has a row in `orchard_received_notes`. In the `pir_notes` table, a canonical note is identified by `canonical_note_id IS NOT NULL` — a foreign key pointing back to the scanner-discovered row. A canonical note gets a `pir_notes` entry when:

- **Nullifier PIR** detects it was spent on-chain (upsert with `is_spent = 1`), or
- **Witness PIR** fetches a Merkle authentication path for it (upsert with `witness_siblings`)

Both paths share a single row via `ON CONFLICT(canonical_note_id) DO UPDATE`, so a note that is both spent-marked and witnessed still has one `pir_notes` entry.

### Provisional Notes

A **provisional note** is one the wallet has *not* yet seen from the scanner. It was discovered ahead of the scanner through trial decryption during change discovery. In the `pir_notes` table, a provisional note is identified by `canonical_note_id IS NULL`. Because no canonical row exists yet, provisional notes carry full note reconstruction data (`diversifier`, `rseed`, `rho`, `cmx`) — enough to reconstruct the Orchard note for spending once a witness is obtained.

Provisional notes arise from the recursive change-discovery flow: when PIR detects that a note was spent at height H, the wallet downloads the compact block at H and trial-decrypts it, discovering one or more change outputs. These change notes are inserted as provisional rows. Since they may themselves have been spent on-chain, their nullifiers are checked in subsequent PIR rounds, potentially producing deeper provisional notes at increasing `depth`.

### How They Interact

```
Scanner-known note (orchard_received_notes)
  │
  ├── PIR detects spend ──► pir_notes row (canonical_note_id = note.id, is_spent = 1)
  │                              │
  │                              ▼ change discovery (trial decrypt block at spend_height)
  │                              │
  │                         Provisional note (canonical_note_id = NULL, depth = 1)
  │                              │
  │                              ├── PIR: unspent → leaf, counts in balance
  │                              │
  │                              └── PIR: spent → mid-chain, discover deeper change
  │                                                   │
  │                                              Provisional note (depth = 2)
  │                                                   └── ...recursive
  │
  └── Scanner catches up to a provisional note's position
        → reconcile: SET canonical_note_id, discovered_by_scanner = 1
          (provisional row is kept, not deleted — descendants remain valid)
```

**In balance calculations:**
- Canonical PIR-witnessed, unspent → `spendable_value`
- Provisional leaf (unspent), witnessed → `spendable_value`
- Provisional leaf (unspent), unwitnessed → `value_pending_spendability`
- Provisional mid-chain (`is_spent = 1`) → excluded (value has flowed to children)
- Scanner-reconciled (`discovered_by_scanner = 1`) → excluded (canonical row now exists; avoids double-counting)

**In coin selection:** witnessed provisional notes are included via a `UNION ALL` alongside canonical notes. The system uses a synthetic txid and negated `pir_notes.id` to distinguish them, reconstructing the Orchard note from stored fields at spend time.

**Lifecycle convergence:** the two categories are not permanent. When the scanner eventually reaches a provisional note's commitment tree position, `reconcile_provisional_for_position` sets `canonical_note_id` and `discovered_by_scanner = 1`, graduating the provisional row to canonical status. The recursive chain (parent/child links) is preserved so all descendants remain valid, and the `is_spent` flag on the same row automatically propagates through `spent_notes_clause`.

## Data Flow

### Nullifier PIR — Recursive Spendability

The recursive spendability design addresses a key problem: when a note is spent, the resulting change output is a *new* note that may itself be spent before the scanner sees it. The wallet must follow this chain to find the actual current balance.

#### Recursive Change Discovery Flow

```
                    Phase 1: Canonical Notes
                    ========================

  ┌─────────────────────────────────────────────────────────┐
  │ 1. getUnspentOrchardNotesForPIR()                       │
  │    → [Note A (100k zats), Note B (50k zats)]            │
  │                                                          │
  │ 2. SpendabilityBackend.checkNullifiersPIR() [network]   │
  │    → [Some(SpendMetadata{height,pos,count}), None]      │
  │    Note A: spent at height H, Note B: unspent           │
  │                                                          │
  │ 3. insertPIRSpentNotes([A.id])                          │
  │    → pir_notes: A marked is_spent=1                     │
  │                                                          │
  │ 4. For each spent note (A):                             │
  │    a. Download compact block at height H                │
  │    b. discoverChangeNotes(A, depth=1, parent=nil)       │
  │       → Trial decrypt → finds change note C (70k)      │
  │       → Extracts tx metadata (hash, fee, time)          │
  │       → Stores activity metadata on A's pir_notes row   │
  │       → Inserts C as provisional (canonical_note_id=NULL)│
  └─────────────────────────────────────────────────────────┘
                            │
                            ▼
                    Phase 2: Recursive Chain
                    ========================

  ┌─────────────────────────────────────────────────────────┐
  │ Loop (up to maxDepth=20):                               │
  │                                                          │
  │ 5. getProvisionalNotesForPIR()                          │
  │    → [C: {nf, value=70k, depth=1}]                     │
  │                                                          │
  │ 6. checkNullifiersPIR([C.nf]) [network, no progress]   │
  │    → C is spent at height H2                            │
  │                                                          │
  │ 7. markProvisionalPIRResults([{C.id, spent=true}])     │
  │                                                          │
  │ 8. For spent provisional C:                             │
  │    a. Download compact block at height H2               │
  │    b. discoverChangeNotes(C, depth=2, parent=C.id)     │
  │       → Trial decrypt → finds note D (50k)             │
  │       → Inserts D as provisional, parent=C              │
  │                                                          │
  │ 9. getProvisionalNotesForPIR()                          │
  │    → [D: {nf, value=50k, depth=2}]                     │
  │                                                          │
  │ 10. checkNullifiersPIR([D.nf]) → D unspent             │
  │ 11. markProvisionalPIRResults([{D.id, spent=false}])   │
  │                                                          │
  │ 12. getProvisionalNotesForPIR() → empty → break        │
  └─────────────────────────────────────────────────────────┘
                            │
                            ▼
                    Result Chain in pir_notes
                    =========================

  Note A (canonical, is_spent=1, spending_tx_hash=TX1)
    └─► Note C (provisional, is_spent=1, depth=1, parent=A's pir_id)
          └─► Note D (provisional, is_spent=0, depth=2, parent=C)
                ↑ This is the current leaf — counts in balance
```

#### Server Contract

The spend-server exposes a YPIR-based nullifier lookup over HTTP. The wallet queries whether specific nullifiers appear on-chain; the server learns which bucket was queried but not which entry, preserving privacy.

#### FFI Layer

The nullifier PIR flow is split across multiple FFI functions to keep network I/O separate from DB access:

**`zcashlc_check_nullifiers_pir`** (in `spendability.rs`) — network-only call:
- Input: JSON array of nullifiers (each a JSON array of 32 bytes), server URL, progress callback
- Output: JSON `NullifierCheckResult { earliest_height, latest_height, spent: [Option<SpendMetadata>] }` where each `SpendMetadata` contains `spend_height`, `first_output_position`, `action_count`
- Connects to the spend-server via `SpendClientBlocking::connect`, checks all nullifiers via `check_nullifiers`

**DB helpers** (in `lib.rs`, accessed through `@DBActor`):
- `zcashlc_get_unspent_orchard_notes_for_pir` — reads unspent Orchard notes (excludes notes already in `orchard_received_note_spends` or marked `is_spent = 1` in `pir_notes`)
- `zcashlc_insert_pir_spent_notes` — upserts into `pir_notes` with `is_spent = 1` (pulls position/value/account from `orchard_received_notes`; skips scan-confirmed spends)
- `zcashlc_get_provisional_notes_for_pir` — returns provisional notes (`canonical_note_id IS NULL`) whose nullifiers have not yet been PIR-checked (`pir_checked = 0`). The `spent_note_id` (root canonical note) is resolved via a recursive CTE that walks `parent_id` links. Returns JSON `[{"id": i64, "nf": [u8], "value": u64, "spent_note_id": i64, "depth": u32}]`
- `zcashlc_mark_provisional_pir_results` — batch-updates provisional notes after PIR nullifier checks. Takes JSON `[{"id": i64, "spent": bool}]`, sets `pir_checked = 1` on each note and `is_spent = MAX(is_spent, :is_spent)` (monotonic — once marked spent, cannot revert)

**Change note discovery** (in `lib.rs`, using helpers from `change_discovery.rs`):
- `zcashlc_discover_change_notes` — given a `spent_note_id`, `depth`, `parent_provisional_id`, and the serialized `CompactBlock` at `spend_height`:
  1. Resolves the account's Orchard FVK from the canonical note
  2. Calls `extract_actions_from_block` to decode the compact block and extract `(position, CompactAction)` pairs plus optional `BlockTxMetadata` (tx hash, fee, block time)
  3. Calls `discover_notes_both_scopes` to trial-decrypt using both internal and external IVK scopes
  4. If metadata is present, calls `set_pir_spending_tx_metadata` on the parent `pir_notes` row, feeding the activity entry system
  5. Inserts each discovered note via `insert_pir_provisional_note`
  6. Returns JSON `[{"position": u64, "value": u64, "provisional_note_id": i64}]`

**Activity entries** (in `lib.rs`, reading from `pir.rs`):
- `zcashlc_get_pir_activity_entries` — runs an aggregating query that groups co-spent canonical notes by `spending_tx_hash` and computes net spend as `gross_value - change_value` (change is the sum of unspent descendant provisional leaves). Returns JSON `[{"tx_hash", "net_value", "gross_value", "change_value", "fee", "height", "block_time"}]`

#### Swift Orchestration

`SDKSynchronizer.checkWalletSpendability(pirServerUrl, progress, maxDepth)`:

**Phase 1 — Canonical notes:**
1. Read unspent notes via `getUnspentOrchardNotesForPIR()` — early return if empty
2. Call `SpendabilityBackend().checkNullifiersPIR()` on a detached task (network, `userInitiated` priority, passes progress callback)
3. Write back spent results via `insertPIRSpentNotes()`
4. Discover depth-1 change notes: for each spent note with `SpendMetadata`, download the compact block at `spendHeight` via `lightWalletService.blockRange`, then call `discoverChangeNotes(depth: 1, parentProvisionalId: nil)`

**Phase 2 — Recursive provisional chain:**
5. Loop up to `maxDepth` iterations (hardcoded to 20 in `SDKSynchronizerLive`):
   a. Read unchecked provisional notes via `getProvisionalNotesForPIR()`
   b. If none remain, the chain is fully resolved — break
   c. PIR-check their nullifiers via `checkNullifiersPIR()` (detached task, no progress callback)
   d. Mark results via `markProvisionalPIRResults()`
   e. For each spent provisional note, download the compact block and call `discoverChangeNotes(depth: provisional.depth + 1, parentProvisionalId: provisional.id)`
6. Return `SpendabilityResult` reflecting only canonical phase-1 data (`earliestHeight`, `latestHeight`, `spentNoteIds`, `totalSpentValue`)

Failures in either phase are per-note — a failed block download or decryption logs with `logger.warn` and does not block other notes. Phase 2 terminates when no more unchecked provisionals exist (chain fully resolved) or the iteration cap is reached.

#### App Trigger

In `RootInitialization.swift`:
- On app startup (foreground only), fires `.checkSpendabilityPIR`
- During sync, refires on `.foundTransactions` and `.syncReachedUpToDate` events
- Gated on `walletConfig.isEnabled(.pirSpendability)`
- Errors are swallowed: logged but result in `checkSpendabilityPIRResult(nil)`, not user-facing alerts

#### Activity View: PIR-Derived Transaction Entries

When PIR detects spent notes and discovers change, the wallet shows synthetic transaction entries in the activity view. These are DB-backed, computed from the `pir_notes` tree rather than cached PIR results.

`zcashlc_get_pir_activity_entries` runs a recursive CTE query:

```sql
WITH RECURSIVE pending_roots AS (
    -- Canonical notes: spent by PIR, have spending_tx_hash, not yet scanner-confirmed
    SELECT pn.id, pn.spending_tx_hash, pn.spending_block_time, pn.spending_fee,
           pn.spend_height, rn.value AS gross_value
    FROM pir_notes pn
    JOIN orchard_received_notes rn ON pn.canonical_note_id = rn.id
    WHERE pn.is_spent = 1
      AND pn.spending_tx_hash IS NOT NULL
      AND NOT EXISTS (
          SELECT 1 FROM orchard_received_note_spends sp
          WHERE sp.orchard_received_note_id = pn.canonical_note_id
      )
),
tree(node_id, tx_hash) AS (
    -- Walk the full descendant tree from each pending root
    SELECT id, spending_tx_hash FROM pending_roots
    UNION ALL
    SELECT child.id, tree.tx_hash
    FROM pir_notes child
    JOIN tree ON child.parent_id = tree.node_id
)
SELECT
    pr.spending_tx_hash AS tx_hash,
    MAX(pr.spending_block_time) AS block_time,
    MAX(pr.spending_fee) AS fee,
    MAX(pr.spend_height) AS height,
    SUM(pr.gross_value) AS gross_value,
    -- Change = sum of unspent provisional leaves in the descendant tree
    COALESCE((
        SELECT SUM(leaf.value)
        FROM tree t
        JOIN pir_notes leaf ON leaf.id = t.node_id
        WHERE t.tx_hash = pr.spending_tx_hash
          AND leaf.is_spent = 0
          AND leaf.canonical_note_id IS NULL
          AND leaf.id NOT IN (SELECT id FROM pending_roots)
    ), 0) AS change_value
FROM pending_roots pr
GROUP BY pr.spending_tx_hash
```

Co-spent canonical notes sharing the same `spending_tx_hash` are grouped into a single entry. The `net_value` (= `gross_value - change_value`) represents what was actually sent. Entries auto-disappear when the scanner inserts `orchard_received_note_spends` for the canonical notes.

In `RootTransactions.swift`, `fetchTransactionsForTheSelectedAccount`:
1. Fetches PIR activity entries via `getPIRActivityEntries()`
2. Constructs `TransactionState` objects (`.paid`, negative net amount) for each entry
3. Merges into the transaction list, deduplicating by tx hash against scanner-confirmed transactions
4. Failures in `getPIRActivityEntries()` are silently caught (`try?`)

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

Queries are issued as a batch via `WitnessClientBlocking::get_witnesses`. If the input is empty, returns immediately without connecting. On failure, the entire batch fails (null return via `catch_panic` + `unwrap_exc_or_null`).

**DB helpers** (in `lib.rs`, accessed through `@DBActor`):
- `zcashlc_get_notes_needing_pir_witness` — reads canonical notes that have a tree position, are unspent, and lack witness data
- `zcashlc_get_provisional_notes_needing_witness` — reads provisional notes that are active leaves (`is_spent = 0`, `discovered_by_scanner = 0`) with a position but no witness
- `zcashlc_get_pir_witness_notes_for_proposal` — extracts Orchard notes selected by a proposal that may need a targeted witness refresh; built from the proposal's shielded inputs, not from a DB query
- `zcashlc_insert_pir_witnesses` — validates each witness by recomputing the root from the note's commitment and siblings; rejects mismatches. Upserts into `pir_notes` (using `ON CONFLICT(canonical_note_id) DO UPDATE`)
- `zcashlc_mark_provisional_note_witnessed` — stores witness data on a provisional note, making it eligible for spendable balance and coin selection
- `zcashlc_get_pir_witnessed_notes` — lists notes with PIR witnesses for diagnostics

#### Swift Orchestration

`SDKSynchronizer.fetchNoteWitnesses(pirServerUrl, progress)`:
1. Record the witness PIR server URL in `SDKFlags` for any later proposal-scoped refresh retry
2. Read notes needing witnesses via `getNotesNeedingPIRWitness()`
3. Call `WitnessBackend().fetchWitnesses()` (network, runs on detached task)
4. Store validated results via `insertPIRWitnesses()`
5. Return `WitnessResult` (witnessed note IDs, total value)

#### App Trigger

In `RootInitialization.swift`:
- Chained after `checkSpendabilityPIRResult` when `pirWitness` is enabled
- Also fired standalone when `pirWitness && !pirSpendability`
- Errors produce `checkWitnessPIRResult(nil)`, not user-facing alerts

#### Transaction Builder Fallback

When the Orchard ShardTree cannot produce a witness (shard incomplete, checkpoint missing), the transaction builder in `zcash_client_backend` automatically falls back to PIR-stored witnesses:

In `build_proposed_transaction`:
1. Attempt `witness_at_checkpoint_id_caching` from the ShardTree
2. If that errors, call `pir_orchard_witness_fallback`
3. Fallback calls `get_pir_merkle_path_by_position` which checks canonical notes first, then provisional notes
4. All PIR witnesses must share the same anchor root — that root becomes the transaction's Orchard anchor
5. If any note lacks a PIR witness, the fallback fails (the note cannot be spent yet)

#### Send-Time Retry on Anchor Mismatch

Witness PIR data can become stale between sync-time fetch and send-time transaction construction. If `createProposedTransactions` fails with the specific Orchard PIR anchor-mismatch errors emitted by Rust, the Swift synchronizer performs a targeted recovery flow:

1. Read the last witness PIR server URL recorded by `fetchNoteWitnesses`
2. Extract only the Orchard notes selected by the current proposal via `getPIRWitnessNotes(for:)`
3. Re-fetch witnesses for just those notes
4. Insert the refreshed witnesses into `pir_notes`
5. Retry transaction construction once

This retry is intentionally narrow:
- It only runs for the known PIR anchor mismatch cases
- It is scoped to proposal-selected Orchard notes, not all notes needing witnesses
- It is skipped if no witness server URL is recorded, the proposal has no eligible Orchard notes, or the refresh returns no witnesses
- A second transaction-construction failure is surfaced unchanged; there is no retry loop

## Database Schema

### `pir_notes` Table (Unified PIR Lifecycle)

A single table tracks the full PIR lifecycle for any note — canonical or provisional: spent state, witness data, recursive change-discovery chain, activity metadata, and scanner reconciliation.

```sql
CREATE TABLE pir_notes (
    id INTEGER PRIMARY KEY,
    canonical_note_id INTEGER UNIQUE
        REFERENCES orchard_received_notes(id) ON DELETE CASCADE,
    account_id INTEGER NOT NULL REFERENCES accounts(id),
    position INTEGER NOT NULL UNIQUE,
    value INTEGER NOT NULL,
    diversifier BLOB,
    rseed BLOB,
    rho BLOB,
    cmx BLOB,
    nullifier BLOB UNIQUE,
    is_spent INTEGER NOT NULL DEFAULT 0,
    spend_height INTEGER,
    witness_siblings BLOB
        CHECK(witness_siblings IS NULL OR length(witness_siblings) = 1024),
    witness_anchor_height INTEGER,
    witness_anchor_root BLOB
        CHECK(witness_anchor_root IS NULL OR length(witness_anchor_root) = 32),
    depth INTEGER NOT NULL DEFAULT 0,
    parent_id INTEGER REFERENCES pir_notes(id),
    pir_checked INTEGER NOT NULL DEFAULT 0,
    discovered_by_scanner INTEGER NOT NULL DEFAULT 0,
    spending_tx_hash BLOB
        CHECK(spending_tx_hash IS NULL OR length(spending_tx_hash) = 32),
    spending_block_time INTEGER,
    spending_fee INTEGER
)
```

The table has two usage modes distinguished by `canonical_note_id`:

**Canonical notes** (`canonical_note_id IS NOT NULL`): Rows linked to an existing `orchard_received_notes` entry. Created when PIR detects a spend (`insert_pir_spent_note` upserts with `is_spent = 1`) or when a witness is fetched (`insert_pir_witness` upserts with witness data). The `ON CONFLICT(canonical_note_id) DO UPDATE` pattern allows both paths to share a single row.

**Provisional notes** (`canonical_note_id IS NULL`): Notes discovered via trial decryption ahead of the scanner. These carry full note reconstruction data (`diversifier`, `rseed`, `rho`, `cmx`) sufficient to reconstruct the note for spending once a witness is obtained.

#### Column reference

- `canonical_note_id`: FK to `orchard_received_notes(id)`. Set when the note has a canonical row (either from initial insert or scanner reconciliation). NULL for provisional-only notes.
- `account_id`: the account that owns this note.
- `position`: the note's global Orchard commitment tree position (unique deduplication key).
- `value`: note value in zatoshis.
- `diversifier`, `rseed`, `rho`, `cmx`: Orchard note fields from trial decryption (provisional only; NULL for canonical-origin rows).
- `nullifier`: the note's nullifier, used for PIR spend-checking.
- `is_spent`: set to 1 when PIR detects the note's nullifier on-chain. Monotonic — once set, cannot revert to 0.
- `spend_height`: the block height at which the spend was detected.
- `witness_siblings`: 1024-byte Merkle authentication path (32 siblings × 32 bytes), obtained via witness PIR. NULL until a witness is fetched.
- `witness_anchor_height`: the chain height the witness is anchored to (server's `tip - CONFIRMATION_DEPTH`).
- `witness_anchor_root`: the 32-byte tree root for self-verification and anchor construction.
- `depth`: hop count in the recursive change-discovery chain (0 = canonical origin, 1 = direct change, 2+ = deeper recursion).
- `parent_id`: self-referential FK linking a change note to the note it was derived from.
- `pir_checked`: set to 1 after this note's nullifier has been checked via PIR.
- `discovered_by_scanner`: set to 1 when the canonical scanner reaches this note's position and reconciles it (along with setting `canonical_note_id`).
- `spending_tx_hash`: the 32-byte hash of the transaction that spent this note. Set during change discovery from `BlockTxMetadata`. Drives the activity entry grouping.
- `spending_block_time`: the block timestamp of the spending transaction.
- `spending_fee`: the transaction fee in zatoshis (NULL if unavailable).

#### Integration points

**`spent_notes_clause`:** When `spendability-pir` is enabled and the table prefix is `"orchard"`, the clause UNIONs `SELECT canonical_note_id FROM pir_notes WHERE canonical_note_id IS NOT NULL AND is_spent = 1` into the spent-notes subquery. This affects all balance and note-selection queries (`get_wallet_summary`, `select_spendable_notes`, etc.).

**`shard_scanned_condition`:** When `spendability-pir` is enabled for Orchard, coin selection accepts notes that have a PIR witness even if their shard is not fully scanned:

```
shard_scanned_condition(Orchard)
    │
    ├── scan_state.max_priority <= :scanned_priority    (original check)
    │
    └── OR EXISTS (SELECT 1 FROM pir_notes pn           (witness PIR bypass)
                   WHERE pn.canonical_note_id = rn.id
                   AND pn.witness_siblings IS NOT NULL)
```

**Provisional notes in coin selection:** When `spendability-pir` is enabled, `select_spendable_notes_matching_value` includes a `UNION ALL` that selects witnessed provisional notes (`canonical_note_id IS NULL`, `witness_siblings IS NOT NULL`, `is_spent = 0`, `discovered_by_scanner = 0`) alongside canonical notes. These use a synthetic txid (zeroblob(32)) with the `pir_notes.id` encoded as the action index. Coin selection logic detects the negative internal note ID (`pir_notes.id` negated) and uses `get_spendable_provisional_note` to reconstruct the Orchard note from stored `diversifier`/`rseed`/`rho` fields.

**`get_wallet_summary` balance:** Provisional notes (`canonical_note_id IS NULL`) contribute to the Orchard balance, filtered to active leaf nodes: `WHERE is_spent = 0 AND discovered_by_scanner = 0`. Mid-chain spent notes (whose value has flowed into deeper change notes) and scanner-reconciled notes (whose canonical row now exists) are excluded. Witnessed notes (`witness_siblings IS NOT NULL`) add to `spendable_value`; unwitnessed notes add to `value_pending_spendability`.

**Scanner reconciliation:** When the canonical scanner inserts an Orchard note via `put_received_note` with a matching `commitment_tree_position`, `reconcile_provisional_for_position` sets `canonical_note_id` and `discovered_by_scanner = 1` on the provisional row rather than deleting it. This preserves the recursive chain — descendants remain valid. The `is_spent` flag on the same row is picked up by `spent_notes_clause` via `canonical_note_id`, so spend status propagates automatically without a separate insert.

- Insert-time witness invariant: before witness data is written, the wallet recomputes the root from the locally stored Orchard note commitment, the note's canonical position, and the provided siblings; if the recomputed root does not equal `anchor_root`, the witness is rejected.
- Refresh invariant: if witness data already exists for the same note, it is replaced only when the incoming `anchor_height` is at least as new as the stored one, so an older snapshot cannot overwrite a newer witness.

#### Lifecycle

```
Canonical note ──> PIR nullifier check ──> upsert pir_notes (is_spent = 1)
                                                   │
                                   ┌───────────────┤
                                   ▼               ▼
                           Scanning finds      Change discovery
                           real spend          (download compact block at spend_height,
                           (UNION deduplicates) trial-decrypt, extract tx metadata)
                                                   │
                                                   ├─► set_pir_spending_tx_metadata
                                                   │   on parent pir_notes row
                                                   │
                                                   ▼
                                            INSERT provisional (canonical_note_id = NULL)
                                            (diversifier, rseed, rho, cmx, nullifier)
                                                   │
                                       ┌───────────┤
                                       ▼           ▼
                                PIR witness   PIR-check nullifier
                                obtained          │
                                (witness_*   ┌────┴────┐
                                 columns)    ▼         ▼
                                       is_spent=0  is_spent=1 ──> Discover deeper
                                       (leaf,      (mid-chain)    change notes
                                        counts                    (repeat at depth+1)
                                        in balance)

Balance contribution:
  • Canonical PIR-witnessed but not spent → spendable_value
  • Provisional leaf, witnessed → spendable_value
  • Provisional leaf, unwitnessed → value_pending_spendability
  • Provisional mid-chain (is_spent=1) → excluded (value in children)
  • Scanner-reconciled (discovered_by_scanner=1) → excluded (canonical row exists)

Activity entry:
  • Canonical spent notes with spending_tx_hash → grouped by tx hash
  • gross_value = SUM(canonical note values in same tx)
  • change_value = SUM(unspent provisional leaf values in descendant tree)
  • net_value = gross_value - change_value
  • Entry disappears when scanner inserts orchard_received_note_spends

Scanner reconciliation:
  Scanner inserts canonical note ──> SET canonical_note_id, discovered_by_scanner=1
  at matching position               (is_spent flag on same row propagates automatically)

Cleared by:
  • truncate_to_height (reorg/rescan)  ──> DELETE FROM pir_notes (unconditional)
  • Account deletion                    ──> FK ON DELETE CASCADE
```

The table is created unconditionally by its migration (schema is identical across all builds). When the corresponding features are off, the table is empty and unused.

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

**PIR bypass:** When `spendability-pir` is enabled, notes with witness data in `pir_notes` (`witness_siblings IS NOT NULL`) bypass this check for Orchard — a PIR witness provides the authentication path that the incomplete shard cannot.

### Combined Effect

```
                              Features OFF             Features ON
                              ────────────             ───────────
get_wallet_summary            is_any_spendable         Orchard: always true
  (balance display)           gates all pools          Sapling: unchanged

select_spendable_notes        unscanned_tip_exists     Orchard: check skipped
  (proposeTransfer)           returns empty vec         Sapling: unchanged

select_spendable_notes        shard_scanned_condition  Orchard: PIR-witnessed
  (coin selection)            requires full shard      notes accepted (canonical
                                                       + provisional via UNION)

getWalletSummary (Swift)      chainTipUpdated gates    Orchard: preserved if
                              both pools               pirCompleted is true
                                                       Sapling: unchanged
```

### Safety with PIR Enabled

- `spent_notes_clause` excludes PIR-marked spent notes (rows in `pir_notes` where `is_spent = 1` and `canonical_note_id IS NOT NULL`) from all balance and selection queries.
- Notes without a spent-marked `pir_notes` row have been confirmed unspent by PIR's on-chain nullifier check.
- Provisional change notes are inserted with `INSERT OR IGNORE` keyed on `position`, making discovery idempotent across retries.
- When the canonical scanner inserts a note at the same position, the provisional row is reconciled by setting `canonical_note_id` and `discovered_by_scanner = 1` (not deleted), preserving the recursive chain. The balance query excludes scanner-reconciled rows, preventing double-counting. The `is_spent` flag on the same row propagates automatically to `spent_notes_clause` via `canonical_note_id`.
- The `is_spent` flag is monotonic — once PIR confirms a note as spent, the flag cannot revert. This prevents inconsistency from retry or out-of-order updates.
- Notes with PIR witnesses (`witness_siblings IS NOT NULL`) can construct valid spend proofs using the server-provided authentication path, which is self-verified against the anchor root.
- Invalid PIR witnesses are rejected before persistence, and stale snapshots do not overwrite newer witness rows.
- If PIR servers are unreachable, `pir_notes` is empty. The wallet falls back to standard scanning behavior — spendability is delayed but never blocked. No funds are lost.
- Newly discovered notes trigger a debounced PIR re-check within 5 seconds via `foundTransactions` / `syncReachedUpToDate`.

## Feature Flag Strategy

Two Cargo features in `zcash_client_sqlite` control PIR integration:

### `spendability-pir`

| Aspect | Feature OFF | Feature ON |
|---|---|---|
| `pir_notes` table | Exists (migration unconditional) | Exists |
| Table contents | Always empty | Populated by PIR |
| `spent_notes_clause` | Original query (no UNION) | UNION with `pir_notes` spent rows |
| `is_any_spendable` (Orchard) | Checked normally | Bypassed (always true) |
| `unscanned_tip_exists` (Orchard) | Checked normally | Bypassed (skipped) |
| `get_wallet_summary` (Orchard) | No provisional note contribution | Active leaf provisional notes (not spent, not scanner-reconciled) added to spendable/pending balance |
| Coin selection (Orchard) | Canonical notes only | Canonical + provisional witnessed notes via `UNION ALL` |
| Scanner reconciliation | No provisional cleanup | Sets `canonical_note_id` and `discovered_by_scanner = 1`; `is_spent` propagates via same row |
| `truncate_to_height` | DELETE is a no-op (empty table) | Clears all `pir_notes` rows |

### `sync-witness-pir`

| Aspect | Feature OFF | Feature ON |
|---|---|---|
| `shard_scanned_condition` (Orchard) | Original check | Accepts notes with `witness_siblings IS NOT NULL` |
| Transaction builder | ShardTree only | Falls back to PIR witnesses from `pir_notes` |
| `truncate_to_height` | (covered by `spendability-pir` above) | Witness data cleared with all `pir_notes` rows |

Both features are enabled together in `zcash-swift-wallet-sdk/Cargo.toml` and disabled by default in `zcash_client_sqlite/Cargo.toml`. The Swift-level `pirCompleted` flag in `SDKFlags` is always compiled in but has no effect unless the Rust layer is built with the features enabled.

## Concurrency Model

Three writers access the wallet SQLite DB:

| Writer | Connection | Writes to |
|---|---|---|
| SDK sync loop | Managed by `@DBActor` | `orchard_received_notes`, `orchard_received_note_spends`, `transactions`, etc. |
| Nullifier PIR DB helpers | Through `@DBActor` | `pir_notes` (spent-note upserts, provisional inserts, pir_checked/is_spent updates, spending_tx metadata) |
| Witness PIR DB helpers | Through `@DBActor` | `pir_notes` (witness data upserts) |

The PIR network calls (`zcashlc_check_nullifiers_pir`, `zcashlc_fetch_pir_witnesses`) open no database connections — they are pure network I/O. DB reads and writes go through the `@DBActor`-managed connection via the `zcashlc_*` helpers in `lib.rs`.

SQLite (even in WAL mode) allows only one writer at a time. The PIR and sync writers target the same `pir_notes` table but use `ON CONFLICT DO UPDATE` upserts and conditional inserts, so there is no logical conflict — only write-lock contention, handled by SQLite's busy retry.

### Race Condition Prevention

PIR and scanning can operate on the same note concurrently. Two layers prevent inconsistency:

1. **Read-time exclusion:** `get_unspent_orchard_notes_for_pir` excludes notes already in `orchard_received_note_spends` or marked `is_spent = 1` in `pir_notes`. `get_notes_needing_pir_witness` excludes notes that already have `witness_siblings IS NOT NULL`. If scanning processes a note before PIR reads, PIR skips it.

2. **Upsert semantics:** PIR inserts use `ON CONFLICT(canonical_note_id) DO UPDATE` so that spent-note recording and witness storage can share rows without conflict. SQLite's write serialization ensures each upsert executes atomically.

## Error Handling

The two PIR subsystems handle failures differently:

**Nullifier PIR** uses batch semantics: `SpendClientBlocking::check_nullifiers` processes all nullifiers in sequence. If the server is unreachable, no results are returned. The wallet degrades gracefully — it shows the balance as-is and the user can retry.

**Witness PIR** uses batch semantics at the network layer: `WitnessClientBlocking::get_witnesses` is called with all positions at once. On failure, the entire batch fails. At the DB layer, each witness is individually validated against the note's commitment before persistence.

At insert time, each returned witness is validated against the wallet's stored Orchard note before it is persisted. At send time, if transaction construction fails because the selected Orchard PIR witnesses disagree on anchor/root, the SDK attempts one targeted refresh-and-retry using the last recorded witness PIR server URL. If no URL is recorded, no selected Orchard notes are eligible, or the refresh yields no witnesses, the original transaction-construction error is returned unchanged.

**Change note discovery** uses per-note semantics within the `checkWalletSpendability` flow. For each spent note, the wallet downloads the compact block at `spend_height` and trial-decrypts locally. If the block download or decryption fails for one note, the error is logged and that note is skipped — other spent notes can still have their change discovered.

**Activity entries** use `try?` at the app layer: if `getPIRActivityEntries()` fails, the transaction list simply shows scanner-confirmed transactions without PIR-derived entries. No error surfaces to the user.

In all cases, server unavailability is non-fatal. The wallet falls back to standard scanning — spendability is delayed but never blocked. Discovery failures do not affect the nullifier PIR results already written to `pir_notes`.

## Cross-Crate Dependency Graph

```
spendability-pir/spend-client ──┐
                                   ├──> zcash-swift-wallet-sdk/rust (libzcashlc)
spendability-pir/witness-client ─┘           │
                                               │ path dependency
                                               ▼
                                        zcash_client_sqlite
                                          └── wallet/pir.rs (unified: spend state,
                                              witness data, provisionals, activity,
                                              Merkle paths, reconciliation)
                                          └── wallet/common.rs (spent_notes_clause,
                                              shard_scanned_condition, provisional
                                              notes in coin selection)
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
