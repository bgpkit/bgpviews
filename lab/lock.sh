#!/usr/bin/env bash
# One archive root has one writer.
#
# Two `run` processes on one root would take the same epoch from `sessions/<client>/next_epoch`
# and open the same `journal.0.bin` with truncate(true), so the second process would erase the
# first one's frames while both reported success. The claim is a file, `archive/.lock`, and what
# this script proves, all through the CLI and the filesystem:
#   1. a second `run` on a held root exits 3 with `writer_conflict`, naming the holder
#   2. `rebuild-index` on that root is refused the same way, because it is the other writer
#   3. read-only commands still run: they take no lock at all
#   4. a claim left by a killed holder is reported, never broken automatically
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

OUT=run-lock
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT/archive"

cat > "$OUT/bgpviews.toml" <<'TOML'
[archive]
root = "run-lock/archive"

[[clients]]
name = "r2"
listen = "127.0.0.1:17951"
allow_from = ["0.0.0.0/0"]
feed = "bmp"

[clients.view]
name = "adj-in-pre"
updates_interval_minutes = 15
rib = true
TOML

# A second client on another port, so the refusal is about the root and not about the port.
cat > "$OUT/bgpviews-other-port.toml" <<'TOML'
[archive]
root = "run-lock/archive"

[[clients]]
name = "r2"
listen = "127.0.0.1:17952"
allow_from = ["0.0.0.0/0"]
feed = "bmp"

[clients.view]
name = "adj-in-pre"
updates_interval_minutes = 15
rib = true
TOML

echo "--- first writer takes the root"
"$BIN" run --config "$OUT/bgpviews.toml" > "$OUT/w1.log" 2>&1 &
PID=$!
for _ in $(seq 1 50); do grep -q listening "$OUT/w1.log" 2>/dev/null && break; sleep 0.1; done
grep -q listening "$OUT/w1.log" || { echo "FAIL: the first writer never listened"; cat "$OUT/w1.log"; exit 1; }
[ -f "$OUT/archive/.lock" ] || { echo "FAIL: no claim file after a successful start"; exit 1; }
echo "  claim: $(cat "$OUT/archive/.lock")"

echo "--- second writer on the same root, different port"
set +e
"$BIN" run --config "$OUT/bgpviews-other-port.toml" > "$OUT/w2.log" 2>&1
rc2=$?
set -e
cat "$OUT/w2.log"
[ "$rc2" -eq 3 ] || { echo "FAIL: second writer exited $rc2, expected 3"; exit 1; }
grep -q "writer_conflict" "$OUT/w2.log" || { echo "FAIL: the refusal does not name writer_conflict"; exit 1; }
grep -q "pid $PID" "$OUT/w2.log" || { echo "FAIL: the refusal does not name the holder's pid ($PID)"; exit 1; }

echo "--- rebuild-index is the other writer, so it is refused too"
set +e
"$BIN" rebuild-index --config "$OUT/bgpviews.toml" > "$OUT/rebuild.log" 2>&1
rcr=$?
set -e
cat "$OUT/rebuild.log"
[ "$rcr" -eq 3 ] || { echo "FAIL: rebuild-index exited $rcr, expected 3"; exit 1; }
grep -q "writer_conflict" "$OUT/rebuild.log" || { echo "FAIL: rebuild-index was not refused as a writer"; exit 1; }

echo "--- read-only commands take no lock"
"$BIN" verify --config "$OUT/bgpviews.toml" > "$OUT/verify.log" 2>&1 || true
if grep -q "writer_conflict" "$OUT/verify.log"; then
  echo "FAIL: verify was blocked by the writer claim"; exit 1
fi
echo "  verify while a writer holds the root: $(head -1 "$OUT/verify.log")"

echo "--- a killed holder leaves a claim on disk, and the next start says so"
kill -9 "$PID"
wait "$PID" 2>/dev/null || true
[ -f "$OUT/archive/.lock" ] || { echo "FAIL: SIGKILL left no record of the dead holder"; exit 1; }
echo "  claim left behind: $(cat "$OUT/archive/.lock")"
"$BIN" run --config "$OUT/bgpviews.toml" > "$OUT/w3.log" 2>&1 &
PID3=$!
for _ in $(seq 1 50); do grep -q listening "$OUT/w3.log" 2>/dev/null && break; sleep 0.1; done
grep -q listening "$OUT/w3.log" || { echo "FAIL: an archive whose writer died could not be reopened"; cat "$OUT/w3.log"; exit 1; }
grep -q "previous writer left a claim" "$OUT/w3.log" || { echo "FAIL: the dead holder was not reported"; cat "$OUT/w3.log"; exit 1; }
echo "  $(grep "previous writer" "$OUT/w3.log")"

echo "--- and a claim whose holder is still alive still refuses, which is the point"
set +e
"$BIN" run --config "$OUT/bgpviews-other-port.toml" > "$OUT/w4.log" 2>&1
rc4=$?
set -e
cat "$OUT/w4.log"
[ "$rc4" -eq 3 ] || { echo "FAIL: the live holder did not block a second writer (exit $rc4)"; exit 1; }
kill -9 "$PID3" 2>/dev/null || true
wait "$PID3" 2>/dev/null || true

echo "ok: one writer per root, live holder refused by name, dead holder reported and reused, read-only unaffected"
