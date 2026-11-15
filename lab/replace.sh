#!/usr/bin/env bash
# A router that redials replaces its session, it does not wait its turn.
#
# The failure this prevents: a second connection accepted by the kernel backlog while the
# first session still holds the archive. The router believes it reconnected; the collector
# keeps writing to the session the router abandoned, and nothing new is archived until the
# old one times out.
#
# The lab produces the redial by restarting r2, which drops its BMP connection and opens a
# new one while the collector's first session is still live.
set -euo pipefail
cd "$(dirname "$0")"

OUT=run-replace
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT/archive"

cat > "$OUT/bgpviews.toml" <<'TOML'
[archive]
root = "run-replace/archive"

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

"$BIN" run --config "$OUT/bgpviews.toml" > "$OUT/daemon.log" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null || true; ./down.sh >/dev/null 2>&1 || true' EXIT
for _ in $(seq 1 50); do grep -q listening "$OUT/daemon.log" 2>/dev/null && break; sleep 0.1; done

echo "--- a silent client holds the session open"
python3 dummy-bmp.py 127.0.0.1 17901 30 &
DUMMY=$!
sleep 2

echo "--- the router then connects, which must replace that session rather than queue"
./up.sh > /dev/null
sleep 10
kill "$DUMMY" 2>/dev/null || true

accepted=$(grep -c "accepted a connection" "$OUT/daemon.log" || true)
replaced=$(grep -c "session replaced by a newer connection" "$OUT/daemon.log" || true)
echo "connections accepted: $accepted, sessions replaced: $replaced"
[ "$accepted" -ge 2 ] || { echo "FAIL: the redial was not accepted while the first session lived"; tail -10 "$OUT/daemon.log"; exit 1; }
[ "$replaced" -ge 1 ] || { echo "FAIL: the first session was not ended by the replacement"; exit 1; }

echo "--- both sessions must have their own epochs and journal parts"
ls "$OUT/archive/sessions/r2/"

./down.sh > /dev/null
kill "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true
trap - EXIT

epochs=$(ls "$OUT/archive/sessions/r2/" | grep -c '^0' || true)
echo "epochs: $epochs"
[ "$epochs" -ge 2 ] || { echo "FAIL: only $epochs session(s) were recorded"; exit 1; }

echo "--- the archive still reconciles"
"$BIN" verify --config "$OUT/bgpviews.toml"
"$BIN" status --config "$OUT/bgpviews.toml" | head -6

echo "ok: a redial replaced the live session, no second writer, archive clean"
