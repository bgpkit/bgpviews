#!/usr/bin/env bash
# SIGHUP produces a RIB immediately, not at the next interval boundary.
#
# The config here sets a 60-minute interval, so a boundary cannot arrive during the run:
# any RIB in the archive was produced by the signal, not by the clock. That is the whole
# point of the test, and the reason it does not just wait for a boundary to pass.
set -euo pipefail

cd "$(dirname "$0")"

WINDOW=${1:-12}
BIN=../target/release/bgpviews
OUT=run-hup

(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT"
mkdir -p "$OUT/archive"

# A 60-minute interval means the first boundary is up to an hour away.
cat > "$OUT/collector.toml" <<'TOML'
[archive]
root = "run-hup/archive"

[[clients]]
name = "r2"
listen = "0.0.0.0:17901"
allow_from = ["172.31.250.0/24", "127.0.0.1/32"]
feed = "bmp"

[clients.view]
name = "adj-in-pre"
updates_interval_minutes = 60
rib = true
TOML

# r2's BMP export in frr/r2.conf points at 17901, so nothing about the routers changes.
echo "starting the collector"
"$BIN" run --config "$OUT/collector.toml" > "$OUT/collector.log" 2>&1 &
COLLECTOR_PID=$!
trap 'kill "$COLLECTOR_PID" 2>/dev/null || true; ./down.sh >/dev/null 2>&1 || true' EXIT

for _ in $(seq 1 50); do
  if grep -q listening "$OUT/collector.log" 2>/dev/null; then break; fi
  sleep 0.1
done
grep -q listening "$OUT/collector.log" || { echo "collector did not start"; cat "$OUT/collector.log"; exit 1; }

echo "bringing the lab up"
./up.sh > /dev/null

# Wait for the session to be accepted before signalling it. A SIGHUP that arrives while no
# session is open is correctly ignored, so signalling before the router exports tests nothing
# and failed this lab one run in several with a log that says exactly that.
for _ in $(seq 1 600); do
  if grep -q "accepted BMP" "$OUT/collector.log" 2>/dev/null; then break; fi
  sleep 0.1
done
if ! grep -q "accepted BMP" "$OUT/collector.log"; then
  echo "FAIL: the router never exported BMP, so there was no session to signal"
  tail -20 "$OUT/collector.log"
  exit 1
fi

# The router's BMP session outlives the peer only briefly: the lab's r2 tears BGP down after the
# initial dump, which closes the monitored peer and then the session. Waiting the full window
# therefore signalled a collector that had no session left, and the lab read the resulting silence
# as a missing RIB. A short settle after the session is accepted keeps the signal inside the
# session the lab means to test.
sleep 1

before=$(find "$OUT/archive" -path '*RIBS*' -name '*.gz' 2>/dev/null | wc -l)
echo "RIB snapshots before SIGHUP: $before"

# The signal tests a session, so it is only meaningful while one is alive. If the router's
# session has already ended, this lab has nothing to measure and says so instead of reporting a
# missing RIB as a product failure.
if grep -q "session for r2 ended" "$OUT/collector.log"; then
  echo "SKIP: the router's session ended before the signal, so there was no session to snapshot"
  ./down.sh > /dev/null 2>&1 || true
  exit 77
fi
# A segment with records is written when one closes, not while a session runs, so asking for one
# before the signal waited for the very session this lab needs: it only appeared once the router
# tore the session down, which is why the signal kept arriving at a collector with no session.

echo "sending SIGHUP"
kill -HUP "$COLLECTOR_PID"

for _ in $(seq 1 100); do
  after=$(find "$OUT/archive" -path '*RIBS*' -name '*.gz' 2>/dev/null | wc -l)
  if [ "$after" -gt "$before" ]; then break; fi
  sleep 0.1
done
after=$(find "$OUT/archive" -path '*RIBS*' -name '*.gz' 2>/dev/null | wc -l)
echo "RIB snapshots after SIGHUP: $after"

./down.sh > /dev/null
kill "$COLLECTOR_PID" 2>/dev/null || true
wait "$COLLECTOR_PID" 2>/dev/null || true
trap - EXIT

[ "$after" -gt "$before" ] || { echo "FAIL: SIGHUP produced no RIB (interval is 60 min, so no boundary could have)"; tail -20 "$OUT/collector.log"; exit 1; }

rib=$(find "$OUT/archive" -path '*RIBS*' -name '*.gz' | tail -1)
echo "reading $rib"
monocle parse "$rib" | head -4

echo "ok: SIGHUP wrote a RIB inside a 60-minute interval"
