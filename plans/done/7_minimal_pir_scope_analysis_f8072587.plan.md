---
name: Minimal PIR scope analysis
overview: Analysis of what PIR components are needed for the minimal goal of "wallet immediately spendable after a month of inactivity," and what can be cut.
todos: []
isProject: false
---

# Minimal PIR Scope for Immediate Spendability

## Your assessment is correct

For the goal "open wallet after inactivity, it is immediately spendable," you need:

- **Nullifier PIR** -- know which of the wallet's existing notes are still unspent
- **Witness PIR** -- get Merkle authentication paths so unspent notes can actually be spent
- **Spendability gate bypasses** -- already implemented, let the wallet show/use balance during sync

You do NOT need recursive change discovery or decryption PIR.

## What you can cut and why

### 1. Recursive change discovery (biggest complexity win)

This is the heart of the complexity in the current design. The entire Phase 2 flow -- following spend chains through change notes -- is unnecessary for the minimal goal.

**Why it's safe to cut:** If note A was spent during the month, and the change went to note C, the wallet simply won't know about C until scanning catches up. The balance will be underreported (missing change notes), but every note the wallet CAN spend will actually be spendable. This is a strictly better UX than today (where nothing is spendable during sync).

**What this removes:**

- Compact block download + trial decryption during PIR (`[change_discovery.rs](zcash-swift-wallet-sdk/rust/src/change_discovery.rs)`)
- `zcashlc_discover_change_notes` FFI
- Provisional notes entirely -- no `canonical_note_id IS NULL` rows in `pir_notes`
- The recursive CTE queries, parent/child tree, depth tracking
- `zcashlc_get_provisional_notes_for_pir`, `zcashlc_mark_provisional_pir_results`
- Provisional note witness fetching (`zcashlc_get_provisional_notes_needing_witness`, `zcashlc_mark_provisional_note_witnessed`)
- Provisional notes in coin selection (the `UNION ALL` in `select_spendable_notes`)
- Provisional notes in balance (`get_wallet_summary` provisional contribution)
- Scanner reconciliation (`reconcile_provisional_for_position`)
- The Phase 2 loop in Swift `checkWalletSpendability` (the `maxDepth` iteration)

### 2. Activity entries (PIR-derived transaction list placeholders)

Without change discovery, there's no `spending_tx_hash`, `spending_block_time`, or `spending_fee` metadata. The recursive CTE for `zcashlc_get_pir_activity_entries` becomes unnecessary. The transaction list just shows scanner-confirmed transactions, which is the current behavior.

### 3. Decryption PIR

Confirmed cut -- it's not fully implemented anyway (only types and extraction exist; no server, client, or FFI).

### 4. Send-time anchor mismatch retry

The targeted witness refresh-and-retry flow when `createProposedTransactions` fails with anchor mismatch can be deferred. If the witness becomes stale between fetch and spend, the user just re-triggers PIR. This removes `zcashlc_get_pir_witness_notes_for_proposal` and the retry logic in Swift.

## What stays (the minimal implementation)

### Server side -- already built, no changes needed

- **Nullifier PIR server** (`spend-server`): Returns `Option(SpendMetadata)` per nullifier. The wallet simply ignores the `SpendMetadata` fields (spend_height, first_output_position, action_count) and treats the result as a boolean "spent or not."
- **Witness PIR server** (`witness-server`): Returns Merkle authentication paths. Used as-is.

### Database -- simplified `pir_notes`

The existing table works, but most columns become unused. Effectively you only populate:

- `canonical_note_id` (always NOT NULL -- no provisional rows)
- `account_id`, `position`, `value`
- `is_spent` (from nullifier PIR)
- `witness_siblings`, `witness_anchor_height`, `witness_anchor_root` (from witness PIR)

Unused columns: `diversifier`, `rseed`, `rho`, `cmx`, `nullifier`, `spend_height`, `depth`, `parent_id`, `pir_checked`, `discovered_by_scanner`, `spending_tx_hash`, `spending_block_time`, `spending_fee`.

### Rust FFI -- 6 functions (down from ~14)


| Keep                                        | Purpose                      |
| ------------------------------------------- | ---------------------------- |
| `zcashlc_get_unspent_orchard_notes_for_pir` | Get notes to check           |
| `zcashlc_check_nullifiers_pir`              | Network: nullifier PIR query |
| `zcashlc_insert_pir_spent_notes`            | Mark spent in DB             |
| `zcashlc_get_notes_needing_pir_witness`     | Get notes needing witnesses  |
| `zcashlc_fetch_pir_witnesses`               | Network: witness PIR query   |
| `zcashlc_insert_pir_witnesses`              | Store witnesses in DB        |


### DB integration points -- keep, but simpler

- `spent_notes_clause`: UNION with `pir_notes` spent rows (canonical only, no provisionals)
- `shard_scanned_condition`: PIR-witnessed notes bypass shard check
- `is_any_spendable` / `unscanned_tip_exists`: Orchard bypasses
- `pir_orchard_witness_fallback`: Transaction builder fallback to PIR witnesses

No provisional notes in coin selection. No provisional balance contribution.

### Swift orchestration -- linear, no loop

```
1. getUnspentOrchardNotesForPIR()       -- which notes to check
2. checkNullifiersPIR(nullifiers)        -- network: are they spent?
3. insertPIRSpentNotes(spentNoteIds)     -- mark spent in DB
4. getNotesNeedingPIRWitness()           -- unspent notes without witnesses
5. fetchWitnesses(positions)             -- network: get Merkle paths
6. insertPIRWitnesses(witnesses)         -- store in DB
Done.
```

No Phase 2 loop, no block downloads, no trial decryption, no depth tracking.

## Tradeoff to be aware of

Without recursive change discovery, the balance will be **underreported** after PIR completes. If 100% of the wallet's known notes were spent during the month (all change went to new notes the wallet doesn't know about yet), the wallet will show **zero spendable balance** until scanning discovers the change notes. The wallet IS more spendable than before (any unspent notes work immediately), but the user might see a temporarily low balance.

This is a reasonable trade for the reduced complexity. Recursive discovery can be added later as a follow-up.

## Summary of scope reduction

```
Full design               Minimal design
-----------               --------------
Nullifier PIR             Nullifier PIR (simple check only)
  + Recursive discovery     (cut)
  + Activity entries        (cut)
  + Provisional notes       (cut)
Witness PIR               Witness PIR
  + Anchor retry            (cut for now)
  + Provisional witnesses   (cut)
Decryption PIR            (cut)
~14 FFI functions         ~6 FFI functions
Complex recursive CTE     Simple queries
Phase 2 loop              Linear flow
```

