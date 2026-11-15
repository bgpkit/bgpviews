#!/usr/bin/env bash
# Does EoR depend on when the collector attaches?
#
# Hypothesis: FRR forwards the monitored peer's end-of-RIB only if the BMP target
# was attached at the moment that peer finished its initial dump. If the collector
# attaches earlier (while the BGP session is still coming up) or later (after the
# dump finished), FRR sends the current table without an EoR.
#
# Four runs, two orders, two repetitions each:
#   collector-first   the collector listens before the lab starts
#   collector-after   the lab is up and the BGP session is Established first
#
# Prints, per run: frames, route records, and how many of those records are EoR
# markers (an UPDATE with no NLRI and no attributes: 23 bytes of BGP for IPv4
# unicast, 29 bytes for IPv6 with MP_UNREACH).
set -euo pipefail

cd "$(dirname "$0")"

WINDOW=${WINDOW:-25}
BIN=../target/release/bgpviews
OUT=run-eor

if [ ! -x "$BIN" ]; then
  echo "building the release binary"
  (cd .. && cargo build --release -q --bin bgpviews)
fi

count_frames() {
  # $1: journal path. Prints "frames records eor"
  python3 - "$1" <<'PY'
import struct, sys

raw = open(sys.argv[1], "rb").read()
off = 8
frames = records = eor = 0
while off < len(raw):
    length = struct.unpack(">I", raw[off + 20:off + 24])[0]
    body = raw[off + 28:off + 28 + length]
    frames += 1
    if body[5] == 0:
        records += 1
        bgp_len = struct.unpack(">H", body[6 + 42 + 16:6 + 42 + 18])[0]
        if bgp_len in (23, 29):
            eor += 1
    off += 28 + length
print(f"{frames}\t{records}\t{eor}")
PY
}

run_once() {
  local order=$1 label=$2
  rm -rf "$OUT/$label"
  mkdir -p "$OUT/$label"

  if [ "$order" = "collector-first" ]; then
    "$BIN" run --config collector.toml > "$OUT/$label/collector.log" 2>&1 &
    COLLECTOR_PID=$!
    for _ in $(seq 1 50); do
      grep -q listening "$OUT/$label/collector.log" 2>/dev/null && break
      sleep 0.1
    done
    ./up.sh > /dev/null
  else
    ./up.sh > /dev/null
    "$BIN" run --config collector.toml > "$OUT/$label/collector.log" 2>&1 &
    COLLECTOR_PID=$!
  fi

  sleep "$WINDOW"
  ./down.sh > /dev/null
  wait "$COLLECTOR_PID" || true

  local journal="" stats
  for candidate in run/archive/sessions/*/*/journal.0.bin; do
    [ -f "$candidate" ] || continue
    journal="$OUT/$label/journal.bin"
    cp "$candidate" "$journal"
    break
  done
  if [ -z "$journal" ]; then
    printf '%-16s %-8s no journal (collector never accepted a session)\n' "$order" "$label"
    rm -rf run/archive
    return
  fi
  stats=$(count_frames "$journal")
  printf '%-16s %-8s frames=%-3s records=%-3s eor=%-3s\n' "$order" "$label" \
    "$(echo "$stats" | cut -f1)" "$(echo "$stats" | cut -f2)" "$(echo "$stats" | cut -f3)"

  # Each run must start from an empty archive or the journal path collides.
  rm -rf run/archive
}

rm -rf "$OUT"
mkdir -p "$OUT"

for rep in a b; do
  run_once collector-first "cf-$rep"
  run_once collector-after "ca-$rep"
done

echo
echo "logs under $OUT/<label>/collector.log"
