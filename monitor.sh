#!/bin/bash
LOG="/root/sync-nullifier-pir/monitor.log"
SERVER_PID=$(pgrep -x spend-server | head -1)
DURATION_MIN=60
INTERVAL_SEC=60
ITERATIONS=$((DURATION_MIN * 60 / INTERVAL_SEC))

echo "=== Spend-server monitor ===" > "$LOG"
echo "Started: $(date -u)" >> "$LOG"
echo "Server PID: $SERVER_PID" >> "$LOG"
echo "Duration: ${DURATION_MIN}min, interval: ${INTERVAL_SEC}s" >> "$LOG"
echo "---" >> "$LOG"

for i in $(seq 1 $ITERATIONS); do
    ts=$(date -u +%H:%M:%S)

    # Check server process
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "$ts | SERVER PROCESS DIED (PID $SERVER_PID)" >> "$LOG"
        echo "EXIT: server crashed at iteration $i" >> "$LOG"
        exit 1
    fi

    # Health endpoint
    health=$(curl -s --max-time 5 http://127.0.0.1:8080/health 2>/dev/null)
    if [ -z "$health" ]; then
        echo "$ts | HEALTH UNREACHABLE" >> "$LOG"
        sleep "$INTERVAL_SEC"
        continue
    fi

    height=$(echo "$health" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('height','?'))" 2>/dev/null)
    nfs=$(echo "$health" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('nullifier_count','?'))" 2>/dev/null)
    phase=$(echo "$health" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('phase','?'))" 2>/dev/null)

    # Memory
    rss=$(ps -o rss= -p "$SERVER_PID" 2>/dev/null | tr -d ' ')
    rss_mb=$((rss / 1024))

    echo "$ts | h=$height nfs=$nfs phase=$phase rss=${rss_mb}MB" >> "$LOG"

    sleep "$INTERVAL_SEC"
done

echo "---" >> "$LOG"
echo "Finished: $(date -u)" >> "$LOG"
echo "RESULT: Server survived ${DURATION_MIN} minutes" >> "$LOG"
