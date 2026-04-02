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

