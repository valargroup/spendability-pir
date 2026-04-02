# Roman's notes

1. If the wallet is synched to tip, PIR is unnecesary, should be good to ignore it.

2. If PIR server is down, the app should continue to work (just no immediate spend). This is an optional feature.

3. If during sync new notes are discovered PIR should be firing for them (i.e. avoid notes that were just synched getting stuck until the tip)