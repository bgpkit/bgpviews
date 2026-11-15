#!/usr/bin/env bash
# A live archive's catalog is current without anyone running rebuild-index.
#
# Two sessions, then DuckDB reads the catalog: every published segment must be there once,
# and running rebuild-index afterwards must not double the rows (it compacts, it does not
# accumulate). Gaps are explicitly out of scope here: they come from the coverage log and
# are derived by rebuild-index, which is stated in docs/features.md.
set -euo pipefail
cd "$(dirname "$0")"

OUT=run-catalog-live
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT/archive"

cat > "$OUT/bgpviews.toml" <<'TOML'
[archive]
root = "run-catalog-live/archive"

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

echo "--- session 1"
"$BIN" run --once --config "$OUT/bgpviews.toml" > "$OUT/s1.log" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null || true; ./down.sh >/dev/null 2>&1 || true' EXIT
for _ in $(seq 1 50); do grep -q listening "$OUT/s1.log" 2>/dev/null && break; sleep 0.1; done
./up.sh > /dev/null
sleep 8
./down.sh > /dev/null
wait "$PID" || true

echo "--- what the catalog says, with no rebuild"
duckdb -c "
  select collector, stream, state, records
  from read_parquet('$OUT/archive/catalog/segments/**/*.parquet')
"
rows=$(duckdb -noheader -list -c "select count(*) from read_parquet('$OUT/archive/catalog/segments/**/*.parquet')")
[ "$rows" -ge 1 ] || { echo "FAIL: the live catalog has no segment rows"; exit 1; }

echo "--- rebuild compacts, and must not duplicate"
"$BIN" rebuild-index --config "$OUT/bgpviews.toml" > /dev/null
after=$(duckdb -noheader -list -c "select count(*) from read_parquet('$OUT/archive/catalog/segments/**/*.parquet')")
[ "$rows" = "$after" ] || { echo "FAIL: live catalog had $rows rows, rebuild produced $after"; exit 1; }

echo "--- the compacted catalog has no live parts left"
left=$(find "$OUT/archive/catalog" -path '*/live/*' -name '*.parquet' | wc -l)
[ "$left" -eq 0 ] || { echo "FAIL: $left live part(s) survived the rebuild"; exit 1; }

kill "$PID" 2>/dev/null || true
trap - EXIT
echo "ok: $rows segment row(s) live, compacted to $after, no duplicates"
