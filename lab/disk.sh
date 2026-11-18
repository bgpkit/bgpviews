#!/usr/bin/env bash
# Disk guards: the derived view goes first, the session goes last.
#
# Filling a real filesystem to 97 % is not a test, it is a hazard, so the level is forced
# through BGPVIEWS_DISK_USED_PERCENT. That seam exists for exactly this and is named so
# nobody mistakes it for a setting.
#
# What must hold at each level:
#   warning   updates keep being written, the RIB is skipped, and the log says why
#   critical  the session closes, and the segment that was open is published as partial
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

OUT=run-disk
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT/archive"

cat > "$OUT/bgpviews.toml" <<'TOML'
[archive]
root = "run-disk/archive"

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

run_case () { # $1 = forced percent, $2 = label
  local percent=$1 label=$2
  echo "--- $label: forcing ${percent}% used"
  BGPVIEWS_DISK_USED_PERCENT="$percent" "$BIN" run --config "$OUT/bgpviews.toml" \
    > "$OUT/$label.log" 2>&1 &
  local pid=$!
  for _ in $(seq 1 50); do grep -q listening "$OUT/$label.log" 2>/dev/null && break; sleep 0.1; done
  ./up.sh > /dev/null
  sleep 6
  # A SIGHUP asks for a RIB, which is the cheapest way to exercise the warning gate.
  kill -HUP "$pid" 2>/dev/null || true
  sleep 2
  ./down.sh > /dev/null
  if kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
  fi
  wait "$pid" 2>/dev/null || true
  echo "  log:"; grep -E "disk|RIB|closing" "$OUT/$label.log" | sed 's/^/    /' || true
}

run_case 91 warning
run_case 98 critical

echo "--- assertions"
grep -q "skipping the RIB" "$OUT/warning.log" \
  || { echo "FAIL: the warning level did not skip the RIB"; exit 1; }
ribs=$(find "$OUT/archive" -path '*/RIBS/*' -name '*.gz' | wc -l)
[ "$ribs" -eq 0 ] || { echo "FAIL: a RIB was written while the disk was at 91%"; exit 1; }

grep -q "disk critical; closing the session" "$OUT/critical.log" \
  || { echo "FAIL: the critical level did not close the session"; exit 1; }
grep -q "session failed: the archive's filesystem reached the critical threshold" "$OUT/critical.log" \
  || { echo "FAIL: the close was not reported as a disk close"; exit 1; }

echo "--- updates still arrived at the warning level, and are absent at critical only because the session closed"
duckdb -c "select stream, state, count(*) from read_parquet('$OUT/archive/catalog/segments/**/*.parquet') group by 1,2"

echo "ok: 91% skipped the RIB, 98% closed the session"
