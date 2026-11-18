#!/usr/bin/env bash
# Does `run` survive a session ending and serve the next one under a new epoch?
#
#   ./daemon.sh [seconds-per-session]
#
# Starts the daemon, brings the lab up and down twice, then stops it. Two sessions
# must produce two epochs, two journal directories and one coverage log whose ranges
# do not overlap.
set -euo pipefail

cd "$(dirname "$0")"
# Only one lab at a time. The labs share the docker network, the fixed ports, the release binary
# and their own run-* directories, so two concurrent runs delete each other's trees and report
# failures that belong to neither. A nested lab inherits the lock through LAB_LOCK_HELD.
if [ "${LAB_LOCK_HELD:-0}" != "1" ]; then
  exec 9>"$PWD/.lab.lock"
  flock -w "${LAB_LOCK_WAIT:-1800}" 9 || {
    echo "SKIP: another lab run holds lab/.lab.lock"
    exit 77
  }
  export LAB_LOCK_HELD=1
fi

WINDOW=${WINDOW:-8}
OUT=run-daemon
BIN=../target/release/bgpviews

(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT"

# The daemon writes to its own archive root: sharing lab/run/archive with live.sh
# would carry a session epoch across experiments and make the epoch numbers look
# wrong when they are only continuing.
sed "s|root = \"run/archive\"|root = \"run-daemon/archive\"|" collector.toml > "$OUT/collector.toml"

"$BIN" run --config "$OUT/collector.toml" > "$OUT/collector.log" 2>&1 &
DAEMON=$!
trap 'kill $DAEMON 2>/dev/null || true; ./down.sh >/dev/null 2>&1 || true' EXIT

for _ in $(seq 1 50); do
  grep -q listening "$OUT/collector.log" 2>/dev/null && break
  sleep 0.1
done
grep -q listening "$OUT/collector.log" || { echo "daemon did not start"; cat "$OUT/collector.log"; exit 1; }

for run in 1 2; do
  echo "session $run: bringing the lab up"
  ./up.sh > /dev/null
  sleep "$WINDOW"
  echo "session $run: taking it down"
  ./down.sh > /dev/null
  sleep 1
done

kill "$DAEMON" 2>/dev/null || true
wait "$DAEMON" 2>/dev/null || true
trap - EXIT

echo
echo "=== daemon log"
grep -E "listening|accepted|epoch|session ended" "$OUT/collector.log" || cat "$OUT/collector.log"

echo
echo "=== journal epochs"
ls -1 "$OUT"/archive/sessions/*/*/journal.0.bin 2>/dev/null |
  sed "s|$OUT/archive/sessions/||"

echo
echo "=== coverage"
cat "$OUT"/archive/coverage/*/updates.jsonl 2>/dev/null

echo
echo "=== segments"
find "$OUT/archive" -name '*.gz' -printf '%f %s bytes\n' 2>/dev/null | sort
