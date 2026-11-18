#!/usr/bin/env bash
# verify reconciles in both directions: every claim has an artifact, every artifact has a claim.
#
# One direction was checked from the start: a coverage range that names a segment must find a
# sidecar whose hash matches. The other one was not: a segment sitting in the archive that no
# coverage range claims is an orphan, and the archive would look complete while holding data it
# cannot place in time.
#
# Two corruptions, each on its own copy, so the finding named is the finding caused:
#   1. remove a sidecar + artifact -> the coverage log names a segment that is gone
#   2. add a stray segment + sidecar -> nothing claims it
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

BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

# Always rebuild: an archive left by an earlier build carries that build's journal bytes, and
# this lab would then report the stale format as corruption in the current one.
echo "creating an archive with two sessions"
rm -rf run-daemon/archive
./daemon.sh > /dev/null 2>&1 || { echo "SKIP: lab/daemon.sh needs docker"; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp -r run-daemon/archive "$work/a"
cat > "$work/bgpviews.toml" <<TOML
[archive]
root = "$work/a"

[[clients]]
name = "r2"
listen = "0.0.0.0:17901"
allow_from = ["172.31.250.0/24"]
feed = "bmp"

[clients.view]
name = "adj-in-pre"
updates_interval_minutes = 15
rib = true
TOML

echo "--- a clean archive must reconcile"
"$BIN" verify --config "$work/bgpviews.toml"

echo "--- 1: delete an artifact and its sidecar, keep the coverage claim"
victim=$(find "$work/a" -path '*UPDATES*' -name '*.gz' | head -1)
echo "  removing $(basename "$victim")"
rm -f "$victim" "$victim.json"
"$BIN" verify --config "$work/bgpviews.toml" > "$work/out1.txt" 2>&1 || true
cat "$work/out1.txt"
grep -q "the coverage log names a segment with no sidecar" "$work/out1.txt" \
  || { echo "FAIL: a dangling coverage claim was not reported"; exit 1; }

echo "--- 2: a stray segment nobody claims"
cp -r run-daemon/archive "$work/b" 2>/dev/null || true
rm -rf "$work/a" && mv "$work/b" "$work/a"
src=$(find "$work/a" -path '*UPDATES*' -name '*.gz' | head -1)
stray="${src%.gz}.9999.gz"
cp "$src" "$stray"
python3 - "$src.json" "${stray}.json" <<'PY'
import json, sys
sidecar = json.load(open(sys.argv[1]))
sidecar["range_start"] = "1999999999.000000"
sidecar["ordinal"] = 9999
sidecar["path"] = sys.argv[2].split("/a/", 1)[1]
open(sys.argv[2], "w").write(json.dumps(sidecar, indent=2) + "\n")
PY
echo "  added $(basename "$stray")"
"$BIN" verify --config "$work/bgpviews.toml" > "$work/out2.txt" 2>&1 || true
cat "$work/out2.txt"
grep -q "no closed coverage range names this segment" "$work/out2.txt" \
  || { echo "FAIL: an orphan segment was not reported"; exit 1; }

echo "--- 3: an MRT whose sidecar is gone, with the coverage claim intact"
rm -rf "$work/a" && cp -r run-daemon/archive "$work/a"
victim=$(find "$work/a" -path '*RIBS*' -name '*.gz' | head -1)
if [ -z "$victim" ]; then
  victim=$(find "$work/a" -path '*UPDATES*' -name '*.gz' | head -1)
fi
# `present` is built from sidecars, so before this check a file with no sidecar was invisible to
# verify in both directions: nothing claimed it and no orphan check could find it.
echo "  removing $(basename "$victim").json"
rm -f "$victim.json"
"$BIN" verify --config "$work/bgpviews.toml" > "$work/out3.txt" 2>&1 || true
cat "$work/out3.txt"
grep -q "this MRT has no sidecar" "$work/out3.txt" \
  || { echo "FAIL: an MRT with no sidecar was not reported"; exit 1; }

echo "--- 4: a peer_state.log cut mid-frame, before anything has repaired it"
rm -rf "$work/a" && cp -r run-daemon/archive "$work/a"
log=$(find "$work/a" -name 'peer_state.log' | head -1)
size=$(stat -c%s "$log")
head -c $((size - 10)) "$log" > "$log.cut" && mv "$log.cut" "$log"
echo "  cut $(basename "$log") from $size to $((size - 10)) bytes"
"$BIN" verify --config "$work/bgpviews.toml" > "$work/out4.txt" 2>&1 || true
cat "$work/out4.txt"
grep -q "ends inside a frame" "$work/out4.txt" \
  || { echo "FAIL: a torn peer_state.log was reported clean"; exit 1; }

echo "ok: both directions reconciled, each finding named, a sidecar-less MRT and a torn log are visible"
