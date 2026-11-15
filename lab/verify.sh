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

BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

if [ ! -d run-daemon/archive ]; then
  echo "creating an archive with two sessions"
  ./daemon.sh > /dev/null 2>&1 || { echo "SKIP: lab/daemon.sh needs docker"; exit 1; }
fi

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

echo "ok: both directions reconciled, each finding named"
