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


