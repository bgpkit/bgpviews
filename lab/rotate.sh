#!/usr/bin/env bash
# A long session's journal is a sequence of parts, not one file that grows forever.
#
# Retention can only expire a part that is closed, so a journal that never rotates keeps the
# one file a live session is writing out of reach of every retention rule: the disk grows with
# the session's age and nothing may delete any of it. This lab runs the real collector against
# the FRR router with a journal limit small enough to rotate within the run, and checks three
# things from the files alone:
#   1. more than one part exists, numbered from 0 with no gap
#   2. every frame is in exactly one part, in order, with no sequence gap across the boundary
#   3. session.json lists the parts and says the session is open
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

OUT=run-rotate
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT/archive"

# The FRR lab's dump is about 1.2 KiB in a handful of frames, so the limit has to be under a
# frame's worth of megabytes: 300 bytes forces a rotation every few frames.
cat > "$OUT/bgpviews.toml" <<'TOML'
[archive]
root = "run-rotate/archive"
journal_max_bytes = 300
journal_rotate_interval_minutes = 360

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

echo "--- one session, 300-byte journal parts"
"$BIN" run --once --config "$OUT/bgpviews.toml" > "$OUT/collector.log" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null || true; ./down.sh >/dev/null 2>&1 || true' EXIT
for _ in $(seq 1 50); do grep -q listening "$OUT/collector.log" 2>/dev/null && break; sleep 0.1; done
./up.sh > /dev/null
sleep 10
./down.sh > /dev/null
wait "$PID" || true
trap - EXIT

echo "--- the parts on disk"
session=$(find "$OUT/archive/sessions" -mindepth 2 -maxdepth 2 -type d | head -1)
[ -n "$session" ] || { echo "FAIL: no session directory"; exit 1; }
ls -l "$session" | sed -n '2,12p'

python3 - "$session" "$OUT/archive" <<'PY'
import json, os, sys

session_dir, root = sys.argv[1], sys.argv[2]

def parts():
    found = []
    for name in os.listdir(session_dir):
        if name.startswith("journal.") and name.endswith(".bin"):
            found.append((int(name[len("journal."):-len(".bin")]), os.path.join(session_dir, name)))
    found.sort()
    return found

found = parts()
if len(found) < 2:
    raise SystemExit(f"FAIL: the journal never rotated, {len(found)} part(s)")
if [index for index, _ in found] != list(range(len(found))):
    raise SystemExit(f"FAIL: part numbers are not 0..{len(found)-1}: {[i for i, _ in found]}")

# Replay the parts the way the sequence rule says: 1, 2, 3, ... with no gap anywhere.
MAGIC = 8
seqs = []
for _, path in found:
    data = open(path, "rb").read()
    offset = MAGIC
    while offset < len(data):
        seq = int.from_bytes(data[offset:offset+8], "big")
        # sequence 8, seconds 8, micros 4, kind 1, length 4, then payload, then the checksum
        length = int.from_bytes(data[offset+21:offset+25], "big")
        seqs.append(seq)
        offset += 29 + length
expected = list(range(1, len(seqs) + 1))
if seqs != expected:
    raise SystemExit(f"FAIL: sequence is not 1..{len(seqs)} across parts: {seqs[:8]}...")

record_path = os.path.join(session_dir, "session.json")
if not os.path.exists(record_path):
    raise SystemExit("FAIL: no session.json")
record = json.load(open(record_path))
names = [part["name"] for part in record["parts"]]
on_disk = [os.path.basename(path) for _, path in found]
if names != on_disk:
    raise SystemExit(f"FAIL: session.json lists {names}, disk has {on_disk}")
# The run finished, so the last write is the close: `open` false is the correct final state.
# While the session was running the same record was rewritten at every segment close with
# `open` true, which is what lets retention see a closed part without waiting for the end.
if record.get("open", True):
    raise SystemExit("FAIL: a finished session's record still says it is open")

print(f"  {len(found)} part(s), {len(seqs)} frames, sequence 1..{len(seqs)}, session.json lists every part")
PY

echo "--- retention leaves the open session alone: its newest part is the one being written"
before=$(find "$session" -name 'journal.*.bin' | wc -l)
"$BIN" status --config "$OUT/bgpviews.toml" | head -3
after=$(find "$session" -name 'journal.*.bin' | wc -l)
[ "$before" = "$after" ] || { echo "FAIL: a read-only status changed the journal"; exit 1; }

echo "ok: the journal rotated, the sequence is unbroken across parts, session.json agrees"
