#!/usr/bin/env bash
# A peer that negotiated ADD-PATH is refused, because the subtype cannot be decided from the OPEN.
#
# The first version of this lab asserted the opposite: it turned RFC 7911 on, expected subtype 8
# or 9, got it, and passed. Reading the prefixes back showed the segment decoded to nothing at
# all, and the bytes say why: FRR with `addpath-tx-all-paths` and one path per prefix sends plain
# NLRI, without the identifier the capability promised. A negotiated capability is a promise
# about a session, not a fact about a message, and a record whose subtype says "path identifier"
# while its NLRI has none is unreadable in a way no checksum catches.
#
# So the lab checks the honest outcome: the session ends with a message naming the families, no
# updates segment is published for it, and nothing anywhere claims a path identifier.
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
sleep 10
./down.sh > /dev/null 2>&1 || true
for _ in $(seq 1 100); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
kill "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true

echo "--- what the collector said"
cat "$OUT/collector.log"

grep -q "accepted BMP" "$OUT/collector.log" \
  || { echo "FAIL: the collector never accepted a BMP session"; exit 1; }
grep -q "ADD-PATH" "$OUT/collector.log" \
  || { echo "FAIL: the collector did not name ADD-PATH in its refusal"; exit 1; }

# Nothing may claim a path identifier, and a refused session must not have published an updates
# segment at all: a segment written for it would be the unreadable case the refusal prevents.
if [ -n "$(find "$OUT/archive" -name 'updates*.gz' -print -quit)" ]; then
  echo "FAIL: an updates segment was published for a refused session"
  find "$OUT/archive" -name 'updates*.gz'
  exit 1
fi
: > "$OUT/types.txt"
for segment in $(find "$OUT/archive" -name '*.gz'); do
  ../target/debug/examples/mrt_types "$segment" >> "$OUT/types.txt"
done
cat "$OUT/types.txt"
if grep -qE 'subtype (8|9):' "$OUT/types.txt"; then
  echo "FAIL: a record claims a path identifier for an ADD-PATH session that was refused"
  exit 1
fi

echo "ok: an ADD-PATH peer is refused by name, no updates segment is published, nothing claims a path identifier"
