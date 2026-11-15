#!/usr/bin/env bash
# Run the real collector against the live lab: TCP, a real router, a real session.
#
#   ./live.sh [seconds]
#
# Starts `bgpviews run`, brings the lab up, lets the session run, tears the lab
# down (which closes the TCP connection), waits for the collector to finish, then
# prints what it archived and what an independent reader makes of it.
set -euo pipefail

cd "$(dirname "$0")"

WINDOW=${1:-12}
BIN=../target/release/bgpviews
OUT=run

# Always build: an incremental cargo build is cheap, and a stale binary silently
# archives with the previous schema, which is exactly the kind of difference this
# harness exists to catch.
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT"

# LIVE_ORDER=after brings the lab up first, which is the case where the router dumps
# to a target that attached to an already-Established peer, so the sidecar's baseline
# should read `observed`.
ORDER=${LIVE_ORDER:-before}
if [ "$ORDER" = "after" ]; then
  echo "bringing the lab up first"
  ./up.sh > /dev/null
fi

echo "starting the collector"
"$BIN" run --once --config collector.toml > "$OUT/collector.log" 2>&1 &
COLLECTOR_PID=$!

# Wait for the listener rather than sleeping a fixed time.
for _ in $(seq 1 50); do
  if grep -q listening "$OUT/collector.log" 2>/dev/null; then break; fi
  sleep 0.1
done
grep -q listening "$OUT/collector.log" || { echo "collector did not start"; cat "$OUT/collector.log"; exit 1; }

if [ "$ORDER" = "before" ]; then
  echo "bringing the lab up"
  ./up.sh > /dev/null
fi

echo "letting the session run for ${WINDOW}s"
sleep "$WINDOW"

echo "tearing the lab down (this closes the BMP connection)"
./down.sh > /dev/null

echo "waiting for the collector to close the session"
wait "$COLLECTOR_PID" || true

echo
echo "=== collector output"
cat "$OUT/collector.log"

echo
echo "=== archive"
find "$OUT/archive" -type f | sed "s|$OUT/archive/||" | sort

echo
echo "=== coverage log"
cat "$OUT"/archive/coverage/*/updates.jsonl

echo
echo "=== independent readers"
for f in $(find "$OUT/archive" -name '*.gz'); do
  echo "$(basename "$f"): monocle $(monocle parse "$f" 2>/dev/null | wc -l) lines, bgpdump $(bgpdump -m "$f" 2>/dev/null | wc -l) lines"
done
