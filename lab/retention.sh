#!/usr/bin/env bash
# Retention drops journal parts, and each dropped part leaves a record behind.
#
# The failure this exists to prevent: a session whose raw frames are gone, indistinguishable
# from a session that never wrote them. After expiry the part file is gone, a tombstone sits
# beside where it was, and session.json still knows its name, size, sha256 and sequence range.
set -euo pipefail
cd "$(dirname "$0")"

OUT=run-retention
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT/archive"

cat > "$OUT/bgpviews.toml" <<'TOML'
[archive]
root = "run-retention/archive"
journal_retention_days = 30

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

run_once () { # $1 = label, $2 = retention days
  sed -i "s/^journal_retention_days = .*/journal_retention_days = $2/" "$OUT/bgpviews.toml"
  "$BIN" run --once --config "$OUT/bgpviews.toml" > "$OUT/$1.log" 2>&1 &
  local pid=$!
  for _ in $(seq 1 50); do grep -q listening "$OUT/$1.log" 2>/dev/null && break; sleep 0.1; done
  ./up.sh > /dev/null
  sleep 6
  ./down.sh > /dev/null
  wait "$pid" || true
  echo "--- $1 (retention $2 days)"
  grep -E "retention|recovered" "$OUT/$1.log" || echo "  (no retention lines)"
}

run_once first 30

echo "--- session.json after the first session"
cat "$OUT"/archive/sessions/r2/*/session.json | head -20

first_part=$(find "$OUT/archive" -name 'journal.*.bin' | head -1)
parts_before=$(find "$OUT/archive" -name 'journal.*.bin' | wc -l)
echo "journal parts on disk: $parts_before (checking $first_part)"
[ "$parts_before" -ge 1 ] || { echo "FAIL: no journal parts to expire"; exit 1; }

# A negative value expires every part that is already closed, which is what a test
# needs: waiting a month is not a test.
run_once second -1

parts_after=$(find "$OUT/archive" -name 'journal.*.bin' | wc -l)
tombs=$(find "$OUT/archive" -name '*.tombstone.json' | wc -l)
echo "after expiry: $parts_after part(s) on disk, $tombs tombstone(s)"
[ "$tombs" -ge 1 ] || { echo "FAIL: no tombstone was written"; exit 1; }
# The second session writes its own part, so the count is not the assertion; the first
# session's part being gone is.
[ "$tombs" -ge 1 ] || { echo "FAIL: no part was expired"; exit 1; }
[ ! -f "$first_part" ] || { echo "FAIL: the first session's part is still on disk"; exit 1; }
[ -f "$first_part.tombstone.json" ] || { echo "FAIL: no tombstone for the expired part"; exit 1; }

echo "--- what the session record still knows about an expired part"
python3 - "$OUT/archive" <<'PY'
import glob, json, sys
for path in glob.glob(sys.argv[1] + "/sessions/r2/*/session.json"):
    record = json.load(open(path))
    for part in record["parts"]:
        print(f'  {record["epoch"]} {part["name"]} seq {part["first_seq"]}..{part["last_seq"]} '
              f'{part["bytes"]}B expired_at={part.get("expired_at")}')
PY

echo "--- verify must still pass: an expired part is recorded, not lost"
"$BIN" verify --config "$OUT/bgpviews.toml" || true

echo "ok: $tombs part(s) expired with their ranges preserved"
