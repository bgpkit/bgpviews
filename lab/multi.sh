#!/usr/bin/env bash
# Two clients, two live sessions, one process.
#
# The bug this exists to catch is structural: `run` used to serve clients in a `for` loop,
# and the first client's accept loop never returns, so a second client was unreachable. A
# unit test cannot see that; two sessions that must be open at the same time can.
#
# One router exports BMP to two ports, so both sessions are live simultaneously rather than
# one after another.
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

OUT=run-multi
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT/archive"

cat > "$OUT/bgpviews.toml" <<'TOML'
[archive]
root = "run-multi/archive"

[[clients]]
name = "r2a"
listen = "0.0.0.0:17901"
allow_from = ["172.31.250.0/24"]
feed = "bmp"

[clients.view]
name = "adj-in-pre"
updates_interval_minutes = 60
rib = true

[[clients]]
name = "r2b"
listen = "0.0.0.0:17903"
allow_from = ["172.31.250.0/24"]
feed = "bmp"

[clients.view]
name = "adj-in-pre"
updates_interval_minutes = 60
rib = true
TOML

echo "--- starting the daemon with two clients"
"$BIN" run --config "$OUT/bgpviews.toml" > "$OUT/daemon.log" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null || true; ./down.sh >/dev/null 2>&1 || true' EXIT

listeners=0
for _ in $(seq 1 60); do
  listeners=$(grep -c listening "$OUT/daemon.log" 2>/dev/null || true)
  [ "$listeners" -ge 2 ] && break
  sleep 0.2
done
[ "$listeners" -ge 2 ] || { echo "FAIL: both listeners were not up"; cat "$OUT/daemon.log"; exit 1; }

echo "--- bringing the router up (it exports to both ports)"
./up.sh > /dev/null
sleep 10

echo "--- both sessions must be live at once"
accepted=$(grep -c "accepted BMP" "$OUT/daemon.log" || true)
echo "accepted sessions: $accepted"
[ "$accepted" -ge 2 ] || { echo "FAIL: only $accepted session(s) were accepted"; grep "accepted" "$OUT/daemon.log"; exit 1; }

echo "--- closing both sessions (a segment is published when an interval or session closes)"
./down.sh > /dev/null
sleep 2
kill "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true

echo "--- both collectors must have written segments"
for c in r2a r2b; do
  n=$(find "$OUT/archive" -path "*${c}-adj-in-pre*" -name '*.gz' | wc -l)
  echo "  $c: $n segment(s)"
  [ "$n" -ge 1 ] || { echo "FAIL: collector $c wrote nothing"; exit 1; }
done

echo "--- the two archives are separate: each coverage log has its own collector"
ls "$OUT/archive/coverage/"

echo "--- verify sees one clean archive with two collectors"
"$BIN" verify --config "$OUT/bgpviews.toml"

trap - EXIT

echo "ok: two clients served concurrently by one process"
