#!/usr/bin/env bash
# A peer that goes quiet after its dump still produces one file per interval.
#
# Section 6 of the design says a healthy but silent period is an empty segment that is written,
# not a hole, "because the writer always knows what to open at a boundary". The collector used to
# close an interval only when the next record arrived or the session ended, so a router with
# nothing to say left its interval open as an unreadable `.tmp` and the intervals after it covered
# by nothing at all. This lab holds a session open and silent for three intervals and asserts the
# files appear while the session is still alive.
#
# The fixture is fed over a real socket instead of through the FRR lab on purpose: the FRR image
# here drops the monitored peer within a second or two of the dump and the BMP session closes with
# it, which would publish the interval through `finish()` and make this lab pass for the wrong
# reason. A sender that stays connected is the only harness where "silent" and "still up" are both
# true.
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

# The collector refuses an interval shorter than a minute, so three intervals is ~3 minutes.
WINDOW=${1:-200}
PORT=${PORT:-17909}
BIN=../target/release/bgpviews
OUT=run-quiet

(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT/archive"

cat > "$OUT/collector.toml" <<TOML
[archive]
root = "$OUT/archive"

[[clients]]
name = "r2"
listen = "0.0.0.0:$PORT"
allow_from = ["127.0.0.1/32"]
feed = "bmp"

[clients.view]
name = "adj-in-pre"
updates_interval_minutes = 1
rib = true
# A RIB is due only at a boundary that is a whole multiple of the RIB interval, and the default
# is 480 minutes, so a lab that expects a snapshot at every minute has to say so.
rib_interval_minutes = 1
TOML

echo "starting the collector"
"$BIN" run --config "$OUT/collector.toml" > "$OUT/collector.log" 2>&1 &
COLLECTOR_PID=$!
trap 'kill "$COLLECTOR_PID" 2>/dev/null || true; kill "$SENDER_PID" 2>/dev/null || true' EXIT

for _ in $(seq 1 100); do
  if grep -q listening "$OUT/collector.log" 2>/dev/null; then break; fi
  sleep 0.1
done
grep -q listening "$OUT/collector.log" || {
  echo "FAIL: the collector did not start"
  cat "$OUT/collector.log"
  exit 1
}

# Feed the committed capture, then stay connected and silent: a router whose table has not changed.
echo "feeding the capture and then saying nothing"
python3 - "$PORT" "$WINDOW" <<'PY' &
import socket, sys, time

port, window = int(sys.argv[1]), float(sys.argv[2])
frames = open("fixtures/frr-8.4-two-router.bin", "rb").read()
sock = socket.create_connection(("127.0.0.1", port))
sock.sendall(frames)
print(f"sent {len(frames)} bytes, holding the session open for {window:.0f}s", flush=True)
time.sleep(window)
sock.close()
PY
SENDER_PID=$!

# Three intervals of wall clock, measured with a deadline rather than a sleep: the assertion is
# that the files appear while the sender is still connected.
deadline=$((SECONDS + WINDOW))
while [ "$SECONDS" -lt "$deadline" ]; do
  if [ -n "$(find "$OUT/archive" -path '*UPDATES*' -name '*.gz' 2>/dev/null | sed -n '2p')" ]; then
    break
  fi
  sleep 1
done
updates=$(find "$OUT/archive" -path '*UPDATES*' -name '*.gz' 2>/dev/null | sort)
ribs=$(find "$OUT/archive" -path '*RIBS*' -name '*.gz' 2>/dev/null | sort)
count_updates=$(printf '%s' "$updates" | grep -c . || true)
count_ribs=$(printf '%s' "$ribs" | grep -c . || true)
echo "closed updates segments: $count_updates"
echo "closed RIB snapshots:    $count_ribs"
printf '%s\n' "$updates"

# The session must still be alive: otherwise `finish()` published the interval and this lab
# proves nothing about the boundary.
if grep -q "session for r2 ended" "$OUT/collector.log"; then
  echo "SKIP: the session ended before the boundary, so the boundary was never tested"
  exit 77
fi

[ "$count_updates" -ge 2 ] || {
  echo "FAIL: no second interval closed while the peer was silent"
  cat "$OUT/collector.log"
  exit 1
}
[ "$count_ribs" -ge 2 ] || {
  echo "FAIL: a RIB is written at every boundary, and only $count_ribs exist"
  cat "$OUT/collector.log"
  exit 1
}

# The later interval carries no records and says so, rather than being an empty file nobody can
# tell apart from a truncated one.
python3 - "$updates" <<'PY'
import json, sys

paths = [p for p in sys.argv[1].split("\n") if p]
empty = 0
for path in paths:
    with open(path + ".json") as handle:
        sidecar = json.load(handle)
    print(f"{path.split('/')[-1]}: records={sidecar['records']} state={sidecar['state']}")
    if sidecar["records"] == 0:
        empty += 1
        assert sidecar["state"] == "complete", "silence is not a failure"
assert empty >= 1, "no interval was published empty, so the peer was not silent"
PY

# `verify` reconciles the archive against itself, and the interval being written has no sidecar
# until it closes: on a live archive that is a note, not a finding.
"$BIN" verify --config "$OUT/collector.toml"
echo "verify exit: $?"

echo "reading the silent interval as a third party would"
empty_file=$(for path in $updates; do
  if python3 -c "import json,sys; sys.exit(0 if json.load(open('$path.json'))['records'] == 0 else 1)"; then
    echo "$path"
    break
  fi
done)
monocle parse "$empty_file" | head -3
echo "ok: a silent interval published, with a RIB and a sidecar that says it is empty"
