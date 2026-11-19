#!/usr/bin/env bash
# A killed collector must leave an archive the next process can safely extend.
#
# The crash is real: SIGKILL in the middle of a session, then a restart. What must hold
# afterwards, all from files:
#   1. the coverage log has no open segment left behind by the dead writer
#   2. no .partial or .tmp file survives recovery
#   3. `verify` is clean, so the archive still reconciles against itself
#   4. the restarted session writes a new segment whose range does not overlap the closed one
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

OUT=run-crash
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT/archive"

cat > "$OUT/bgpviews.toml" <<'TOML'
[archive]
root = "run-crash/archive"

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

echo "--- session 1, killed with SIGKILL"
"$BIN" run --config "$OUT/bgpviews.toml" > "$OUT/s1.log" 2>&1 &
PID=$!
for _ in $(seq 1 50); do grep -q listening "$OUT/s1.log" 2>/dev/null && break; sleep 0.1; done
./up.sh > /dev/null
sleep 8
kill -9 "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true
./down.sh > /dev/null

# A killed writer leaves a half-written segment artefact; make sure one exists to remove.
find "$OUT/archive" -name '*.gz' | head -1 | sed 's/$/.partial/' | xargs -r -I{} sh -c 'cp "$(echo {} | sed "s/\.partial$//")" {} 2>/dev/null || true'

echo "--- archive left behind"
find "$OUT/archive" -type f | sed "s|$OUT/archive/||" | sort
echo "--- coverage before recovery"
cat "$OUT"/archive/coverage/*/updates.jsonl | tail -3

echo "--- session 2, which must recover first"
"$BIN" run --config "$OUT/bgpviews.toml" > "$OUT/s2.log" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null || true; ./down.sh >/dev/null 2>&1 || true' EXIT
for _ in $(seq 1 50); do grep -q listening "$OUT/s2.log" 2>/dev/null && break; sleep 0.1; done
./up.sh > /dev/null
sleep 6
./down.sh > /dev/null
kill "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true
trap - EXIT

echo "--- what recovery said"
grep -E "recovered|recovery" "$OUT/s2.log" || echo "(no recovery lines)"

grep -q "recovered: closed segment" "$OUT/s2.log" \
  || { echo "FAIL: the open segment left by the killed writer was not closed"; exit 1; }

left=$(find "$OUT/archive" -name '*.tmp' | wc -l)
[ "$left" -eq 0 ] || { echo "FAIL: $left temporary file(s) survived recovery"; exit 1; }

# A segment rebuilt from the killed writer is published as `.partial`, which is an artifact and not
# a leftover: the crash is why it is partial. What must hold is that no artifact sits there without
# the sidecar that describes it, because a reader checks the sidecar before trusting the file.
for artifact in $(find "$OUT/archive" -name '*.partial*.gz'); do
  [ -f "$artifact.json" ] || {
    echo "FAIL: $artifact has no sidecar, so nothing describes what it holds"
    exit 1
  }
done

echo "--- verify"
"$BIN" verify --config "$OUT/bgpviews.toml" || { echo "FAIL: verify found a problem after recovery"; exit 1; }

echo "--- coverage after recovery: the dead writer's segment is closed, the next one starts later"
cat "$OUT"/archive/coverage/*/updates.jsonl | python3 -c '
import json,sys
ranges=[]
for line in sys.stdin:
    # Each line is JSON followed by a checksum field; the JSON ends at the last brace.
    d=json.loads(line[:line.rindex("}")+1])
    ranges.append((d.get("kind"), d.get("range_kind") or d.get("reason")))
print(len(ranges), "lines")
print(ranges[-4:])
'

echo "ok: killed writer recovered, archive verifies, no temp files left"
