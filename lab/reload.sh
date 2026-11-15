#!/usr/bin/env bash
# A live SIGHUP reload applies only what a running session can honestly change.
#
# Two layers of evidence:
#   1. the unit tests in crates/bgpviews/src/reload.rs: rib flips apply, identity changes
#      are refused by name, and a broken file leaves the running configuration alone
#   2. a real daemon against the FRR lab: change rib=false in the file, send SIGHUP, and
#      check the log says it applied, then change the port and check it is refused
set -euo pipefail
cd "$(dirname "$0")"

cargo test -q --bin bgpviews reload 2>&1 | tail -3

BIN=../target/release/bgpviews
OUT=run-reload
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT/archive"

cat > "$OUT/bgpviews.toml" <<'TOML'
[archive]
root = "run-reload/archive"

[[clients]]
name = "r2"
listen = "0.0.0.0:17901"
allow_from = ["172.31.250.0/24"]
feed = "bmp"

[clients.view]
name = "adj-in-pre"
updates_interval_minutes = 60
rib = true
TOML

"$BIN" run --config "$OUT/bgpviews.toml" > "$OUT/collector.log" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null || true; ./down.sh >/dev/null 2>&1 || true' EXIT

for _ in $(seq 1 50); do
  grep -q listening "$OUT/collector.log" 2>/dev/null && break
  sleep 0.1
done
./up.sh > /dev/null
sleep 8

echo "--- reload 1: rib = false, and a new listen port"
sed -i 's/^rib = true/rib = false/; s/17901/17902/' "$OUT/bgpviews.toml"
kill -HUP "$PID"
sleep 2

grep -q "SIGHUP applied rib = false" "$OUT/collector.log" \
  || { echo "FAIL: the rib change was not applied"; tail -10 "$OUT/collector.log"; exit 1; }
grep -q "SIGHUP refused: listen 0.0.0.0:17901 -> 0.0.0.0:17902" "$OUT/collector.log" \
  || { echo "FAIL: the listen change was not refused"; tail -10 "$OUT/collector.log"; exit 1; }
grep -q "RIB writing is disabled; nothing to snapshot" "$OUT/collector.log" \
  || { echo "FAIL: a HUP with rib disabled still tried to snapshot"; tail -10 "$OUT/collector.log"; exit 1; }

echo "--- reload 2: a broken file must not stop the daemon"
printf 'not toml = = =\n' > "$OUT/bgpviews.toml"
kill -HUP "$PID"
sleep 1
grep -q "SIGHUP kept the running configuration" "$OUT/collector.log" \
  || { echo "FAIL: a broken file was not reported"; tail -10 "$OUT/collector.log"; exit 1; }

kill -0 "$PID" || { echo "FAIL: the daemon died"; exit 1; }
echo "--- collector log"
grep SIGHUP "$OUT/collector.log"

kill "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true
./down.sh > /dev/null
trap - EXIT
echo "ok: rib applied live, identity changes refused, broken file survived"
