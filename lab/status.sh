#!/usr/bin/env bash
# `status` answers "what does this archive contain, and where are the holes" from the files alone.
#
# Three things are checked, from the output and from the disk:
#   1. the four blocks a reader needs are present: collector, segments with state counts,
#      partial reasons, and the baseline per family
#   2. reading the archive does not write to it: the journal's part count is the same before and
#      after, because a status that repaired something would be a writer with no lock
#   3. `status | head -1` exits 0, because a reader that stops early is normal and a panic is not
#
# The archive comes from lab/daemon.sh when there is a router to talk to, and from the copy
# committed under lab/run-daemon/archive otherwise.
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

OUT=run-status
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT"

if [ -d run-daemon/archive ]; then
  cp -r run-daemon/archive "$OUT/archive"
elif ./daemon.sh > /dev/null 2>&1; then
  cp -r run-daemon/archive "$OUT/archive"
else
  echo "SKIP: no archive under lab/run-daemon and no docker to make one"
  exit 77
fi

cat > "$OUT/bgpviews.toml" <<TOML
[archive]
root = "$OUT/archive"

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

echo "--- status"
"$BIN" status --config "$OUT/bgpviews.toml" | tee "$OUT/status.txt"

grep -q '^collector ' "$OUT/status.txt" || { echo "FAIL: no collector line"; exit 1; }
grep -Eq 'segments [0-9]+ \(complete [0-9]+, partial [0-9]+, withheld [0-9]+\)' \
  "$OUT/status.txt" || { echo "FAIL: no segment count by state"; exit 1; }
grep -q 'partial reasons' "$OUT/status.txt" || { echo "FAIL: no partial reasons"; exit 1; }
grep -q 'baseline ' "$OUT/status.txt" || { echo "FAIL: no baseline line"; exit 1; }

echo "--- reading the archive must not change it"
session=$(find "$OUT/archive/sessions" -mindepth 2 -maxdepth 2 -type d | head -1)
[ -n "$session" ] || { echo "FAIL: no session directory"; exit 1; }
before=$(find "$session" -name 'journal.*.bin' | wc -l)
before_sum=$(find "$session" -name 'journal.*.bin' -exec sha256sum {} + | sort | sha256sum)
"$BIN" status --config "$OUT/bgpviews.toml" > /dev/null
after=$(find "$session" -name 'journal.*.bin' | wc -l)
after_sum=$(find "$session" -name 'journal.*.bin' -exec sha256sum {} + | sort | sha256sum)
[ "$before" = "$after" ] || { echo "FAIL: status changed the journal part count"; exit 1; }
[ "$before_sum" = "$after_sum" ] || { echo "FAIL: status changed a journal part"; exit 1; }

echo "--- a reader that stops early is not an error"
set +e
"$BIN" status --config "$OUT/bgpviews.toml" 2> "$OUT/head.err" | head -1 > /dev/null
code=${PIPESTATUS[0]}
set -e
[ "$code" -eq 0 ] || { echo "FAIL: status | head -1 exited $code"; exit 1; }
[ ! -s "$OUT/head.err" ] || { echo "FAIL: status wrote to stderr when its reader left:"; cat "$OUT/head.err"; exit 1; }

echo "ok: status reports the archive from its files, changes nothing, and survives a closed reader"
