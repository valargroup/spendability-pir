# Roman's notes

1. If the wallet is synched to tip, PIR is unnecesary, should be good to ignore it.

2. If PIR server is down, the app should continue to work (just no immediate spend). This is an optional feature.

3. If during sync new notes are discovered PIR should be firing for them (i.e. avoid notes that were just synched getting stuck until the tip)


## How does scan happen currently:

Two things need to be proven:
- A valid note exists
- It is unspent

1. Sync wallet from birthday height
2. Note is found. Continue scanning.
3. Trigger prioritization of the note commitment tree witness construction for the found note.
4. 30s-1m later (could be more), it becomes spendable by constructing the witness. There is a single thread that scans ranges.
5. Wallet does not know if the nullifier was spent.
6. If you try spending and nullifier was spent, you get an RPC error.

Solution:
1. To prevent RPC errors, query a PIR server that immediately tells you whether a note is spent without revealing the nullifier
2. To make notes immediately spendable, prioritize witness construction for the discovered notes.

## Rationale

- Separate table for `pir_spent_notes`. If it did not exist, we would have to either add a column to `orchard_received_note_spends ` or enter sentinels into transactions table.

The FK constraint (REFERENCES transactions(id_tx) ON DELETE CASCADE) means you can't just put a -1 in there. You need an actual row in transactions. That row needs a txid BLOB NOT NULL UNIQUE — so you'd need a synthetic txid (e.g. a deterministic hash or a zeroed-out blob). And to pass tx_unexpired_condition, the simplest option

## UX Problems


### Problem 1: Large Note Spent for Small Amount, Balance Decreases Significantly

* After PIR, balances may update to a lower value thatn actual. I suspect that the reason is because a large note was spent instead of a small note. only when full sync finished, did the balance increase back. We should clearly communicate that via UI (instead of subtracting these from balance with no other feedback


### Problem 2: The detected spend entry stays until full sync

  I noticed that we added a PIR detected spend state

@TransactionState.swift (354-365) 

It is only removed until scanning catches up.

This leads to the following problem:
1. A note is detected
2. PIR is queried. Detects note spent
2. Detected spend entry appears. Stays until full sync
3. Few blocks later we detect the note being spent, create an activity entry for its remoal
4. But the detected spend entry still stays. We could have removed it at step 3.

Please meake detected spend entries be note-aware so that we can cleanly update statuses

## Wallet Flow

1. Canonical

These are the notes identified during sync.

2. Provisional

These are the intermediary notes identified as part of recursive PIR flow

### Canonical Note Scanning Identification

1. Scanning flow identifies the notes
2. Scanning flow trial decrypts and persists the notes belonging to them

Spendability PIR wallet update needed:

1. During scanning, trigger PIR processing for it.
2. When starting up after restart, trigger PIR processing for all notes in the wallet.

### Recursive PIR Flow

#### Find Unspent Notes

1. Check nullifier PIR.
   * If the note is unspent, we stop here, the note is in balance, add it to unspent notes.
   * If the note is spent, go to 2.
2. Find the transaction that spent the note
   * Query the rpc for the block where the spend happenned (to be replaced by PIR in the future)
   * Identify the specific actions for spend and for change.
      * Spend is outgoing so it is not included in the balance
      * For change note, proceed to 1.

#### Create Witnesses

Once we "Find Unspent Notes", begin the witness creation for them.

### Design Goals

* The PIR system is an optional sidecar that improves UX. If PIR server is donw, the system fallbacks to the standard scanning flow
* As an outcome of the above, try to decouple databases tables

### Edge Cases

* During scanning, we update the canonical balance and then there are duplicate transactions that break activity.
  * TODO: should we make scanning flow be the canonical for display as it catches up through heights. OR should we check if we are in PIR mode at sync start and, if so, make PIR be the canonical for the view until the full catch up

### Design Claims

This section documents statements as factual.

#### Recursive Handling of 2 Notes Going Into Same Spend 

Problem: Scanning detects a change note, triggers PIR spendability for it. But it was already scanned by the recursive spendabiliy chain triggerred by the note before it.
   * Before we keep doing redundant recursion, confirm if it was already processed and stop early

Imagine
- Canonical note A
- Canonical note B
- Provisional note C
- Provisional note D

2-in-2-out where both A and B are spent in the same transaction. C and D are outputs.

Then, if we were to start from A, recursion would go through the spend and continue recursing.

But note B would stop at the same spend because the downstream recursion is alrady marked completed by A. 

Any further recursion on C and D from B's path is deduplicated by the pir_checked = 0. The activity view groups by spending_tx_hash, so A and B collapse into one entry with gross_value = A.value + B.value and change_value = C.value + D.value.



# TODOS Pre-merge

zcash-swift-wallet-sdk
- Delete the DEBUG only APIs
- Comment on SQL connection addition as drive by in PR
- Remove debug reset APIs


# Design Decisions

- Avoid Keystone Hardware wallet support. Avoid breaking PCZT APIs.



