#!/usr/bin/env bash
# Backpressure: a stall gets a name, and a long enough one closes the session.
#
# There is no queue in this collector, so the only way it falls behind is a write that
# stalls. The guard measures that, marks the open interval `backpressure`, and after a
# deadline closes the session instead of falling further behind.
#
# The stall is forced through BGPVIEWS_FRAME_DELAY_MS, a test seam named so it cannot be
# mistaken for a setting: a real test would need a filesystem that misbehaves on cue.
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

OUT=run-backpressure
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT/archive"

cat > "$OUT/bgpviews.toml" <<'TOML'
[archive]
root = "run-backpressure/archive"
write_stall_ms = 50
backpressure_deadline_seconds = 1

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

echo "--- a stalled collector: every frame is made to take 120 ms, deadline is 1 s"
BGPVIEWS_FRAME_DELAY_MS=120 "$BIN" run --once --config "$OUT/bgpviews.toml" > "$OUT/stalled.log" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null || true; ./down.sh >/dev/null 2>&1 || true' EXIT
for _ in $(seq 1 50); do grep -q listening "$OUT/stalled.log" 2>/dev/null && break; sleep 0.1; done
./up.sh > /dev/null
sleep 10
./down.sh > /dev/null
wait "$PID" || true
trap - EXIT

echo "--- what the collector said"
grep -E "stall|backpressure|session failed" "$OUT/stalled.log" | head -5 || true

echo "--- the interval that stalled must carry the name"
"$BIN" status --config "$OUT/bgpviews.toml" | sed -n '1,6p'

python3 - "$OUT/archive" <<'PY'
import glob, json, sys
reasons = set()
for path in glob.glob(sys.argv[1] + "/*/bgpdata/*/*/*.json"):
    sidecar = json.load(open(path))
    reasons.update(sidecar.get("partial_reasons", []))
print("reasons seen:", sorted(reasons))
if "backpressure" not in reasons:
    raise SystemExit("FAIL: no segment recorded the backpressure reason")
print("ok: a stalled interval is named backpressure")
PY

echo "--- a healthy collector must not carry the reason"
rm -rf "$OUT/healthy"
mkdir -p "$OUT/healthy/archive"
sed "s|run-backpressure/archive|run-backpressure/healthy/archive|" "$OUT/bgpviews.toml" > "$OUT/healthy/bgpviews.toml"
"$BIN" run --once --config "$OUT/healthy/bgpviews.toml" > "$OUT/healthy.log" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null || true; ./down.sh >/dev/null 2>&1 || true' EXIT
for _ in $(seq 1 50); do grep -q listening "$OUT/healthy.log" 2>/dev/null && break; sleep 0.1; done
./up.sh > /dev/null
sleep 8
./down.sh > /dev/null
wait "$PID" || true
trap - EXIT

python3 - "$OUT/healthy/archive" <<'PY'
import glob, json, sys
reasons = set()
for path in glob.glob(sys.argv[1] + "/*/bgpdata/*/*/*.json"):
    reasons.update(json.load(open(path)).get("partial_reasons", []))
print("healthy reasons:", sorted(reasons))
if "backpressure" in reasons:
    raise SystemExit("FAIL: a healthy session was marked backpressure")
print("ok: no backpressure on a healthy session")
PY

echo "ok: stall named, deadline closes the session, healthy run clean"
