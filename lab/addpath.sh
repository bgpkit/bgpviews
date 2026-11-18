#!/usr/bin/env bash
# An ADD-PATH peer's records must be written under the RFC 8050 subtype.
#
# The subtype is the whole contract: an add-path record and a plain one are the same shape apart
# from four octets inside every NLRI, so a reader told "plain" takes the path identifier for a
# prefix length and produces prefixes that never existed. Nothing in the file contradicts it.
#
# The lab brings up the same two FRR routers with RFC 7911 turned on (r1 sends every path with an
# identifier, r2 accepts them), collects for a few seconds, and then reads the archive back with
# bgpkit-parser: every updates record must be subtype 8 or 9, and none may be the plain 1 or 4.
set -euo pipefail
cd "$(dirname "$0")"

OUT=run-addpath
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews && cargo build -q --example mrt_types)

rm -rf "$OUT"
mkdir -p "$OUT/archive"

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
updates_interval_minutes = 60
rib = false
TOML

FRR_CONF_DIR=frr-addpath ./up.sh > "$OUT/up.log" 2>&1 || {
  echo "SKIP: the FRR lab did not come up: $(tail -2 "$OUT/up.log" | tr '\n' ' ')"
  exit 77
}
trap './down.sh >/dev/null 2>&1 || true' EXIT

"$BIN" run --once --config "$OUT/bgpviews.toml" > "$OUT/collector.log" 2>&1 &
PID=$!
for _ in $(seq 1 50); do grep -q listening "$OUT/collector.log" 2>/dev/null && break; sleep 0.1; done

# Let the session establish and send its initial table. Then take the router away: the
# collector publishes on a clean session end, and `--once` returns after that one session.
sleep 12
./down.sh > /dev/null 2>&1 || true
for _ in $(seq 1 100); do
  kill -0 "$PID" 2>/dev/null || break
  sleep 0.2
done
kill "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true

grep -q "accepted BMP" "$OUT/collector.log" \
  || { echo "FAIL: the collector never accepted a BMP session"; tail -5 "$OUT/collector.log"; exit 1; }

segment=$(find "$OUT/archive" -name 'updates*.gz' | head -1)
[ -n "$segment" ] || { echo "FAIL: no updates segment was written"; exit 1; }

echo "--- what the segment claims"
../target/debug/examples/mrt_types "$segment" | tee "$OUT/types.txt"

if grep -qE 'type 17 subtype (1|4):' "$OUT/types.txt"; then
  echo "FAIL: an ADD-PATH session was recorded under the plain subtype"
  exit 1
fi
grep -qE 'type 17 subtype (8|9):' "$OUT/types.txt" \
  || { echo "FAIL: no ADD-PATH subtype was written"; exit 1; }

# And the ecosystem's reader must agree: path identifiers parsed as path identifiers.
../target/debug/examples/mrt_types "$segment" > /dev/null \
  || { echo "FAIL: bgpkit-parser could not read the segment"; exit 1; }

echo "ok: an ADD-PATH peer's updates carry subtype 8 or 9, and bgpkit-parser reads them"
