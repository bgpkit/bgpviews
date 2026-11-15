#!/usr/bin/env bash
# Withholding a corrupt segment, and what the archive says afterwards.
#
# The point is the difference between "gone" and "withheld". A deleted segment would leave
# the coverage log claiming records that no longer exist; a withheld one leaves a tombstone,
# a moved artifact a human can still inspect, and a verify run that stays clean.
set -euo pipefail
cd "$(dirname "$0")"

OUT=run-quarantine
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT/archive"
cp -r run-daemon/archive/. "$OUT/archive/" 2>/dev/null || ./daemon.sh > /dev/null 2>&1 && cp -r run-daemon/archive/. "$OUT/archive/"

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

target=$(find "$OUT/archive" -path '*UPDATES*' -name '*.gz' | head -1)
[ -n "$target" ] || { echo "SKIP: no archive to work with"; exit 0; }
echo "--- corrupting $target"
printf '\x00' | dd of="$target" bs=1 seek=20 conv=notrunc 2>/dev/null

echo "--- verify before quarantine"
"$BIN" verify --config "$OUT/bgpviews.toml" || true

echo "--- quarantine"
"$BIN" quarantine --config "$OUT/bgpviews.toml" --path "$target" --reason sha256_mismatch

echo "--- verify after quarantine"
"$BIN" verify --config "$OUT/bgpviews.toml"
[ "$(find "$OUT/archive" -path '*tombstones*' -name '*.json' | wc -l)" -ge 1 ] \
  || { echo "FAIL: no tombstone was written"; exit 1; }
[ -f "$OUT/archive/quarantine/r2-adj-in-pre/$(basename "$target")" ] \
  || { echo "FAIL: the artifact was not moved into quarantine/"; exit 1; }
[ ! -f "$target" ] || { echo "FAIL: the artifact is still at its published path"; exit 1; }

echo "--- tombstone"
cat "$OUT"/archive/tombstones/*/*.json

echo "--- a second verify must not resurrect the finding"
"$BIN" verify --config "$OUT/bgpviews.toml"

echo "ok: segment withheld, tombstone written, verify clean"
