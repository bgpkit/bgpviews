#!/usr/bin/env bash
# Control for the EoR ordering question: the same two orders, but captured by the
# independent Python listener instead of by bgpviews.
#
# If the Python capture also has no EoR when it listens before the lab starts, the
# behaviour belongs to FRR, not to our collector.
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

WINDOW=${WINDOW:-25}
OUT=run-eor-control

count_capture() {
  python3 - "$1" <<'PY'
import struct, sys

raw = open(sys.argv[1], "rb").read()
off = frames = records = eor = 0
while off < len(raw):
    length = struct.unpack(">I", raw[off + 1:off + 5])[0]
    body = raw[off:off + length]
    frames += 1
    if body[5] == 0:
        records += 1
        bgp_len = struct.unpack(">H", body[6 + 42 + 16:6 + 42 + 18])[0]
        if bgp_len in (23, 29):
            eor += 1
    off += length
print(f"{frames}\t{records}\t{eor}")
PY
}

run_once() {
  local order=$1 label=$2
  mkdir -p "$OUT"

  if [ "$order" = "listener-first" ]; then
    python3 ./bmp-listen.py "$OUT/$label.bin" "$WINDOW" 17901 172.31.250.12 > "$OUT/$label.log" 2>&1 &
    CAPTURE_PID=$!
    sleep 2
    ./up.sh > /dev/null
  else
    ./up.sh > /dev/null
    python3 ./bmp-listen.py "$OUT/$label.bin" "$WINDOW" 17901 172.31.250.12 > "$OUT/$label.log" 2>&1 &
    CAPTURE_PID=$!
  fi

  wait "$CAPTURE_PID" || true
  ./down.sh > /dev/null

  local stats
  stats=$(count_capture "$OUT/$label.bin")
  printf '%-15s %-8s frames=%-3s records=%-3s eor=%-3s\n' "$order" "$label" \
    "$(echo "$stats" | cut -f1)" "$(echo "$stats" | cut -f2)" "$(echo "$stats" | cut -f3)"
}

rm -rf "$OUT"
for rep in a b; do
  run_once listener-first "lf-$rep"
  run_once listener-after "la-$rep"
done
