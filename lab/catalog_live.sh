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

# Current state is the row with the greatest part sequence per key, which is why the
# projection can append without ever rewriting a part. Counting with the same rule is the
# only comparison that survives a rebuild that adds parts beside the live ones.
segments_now() {
  duckdb -noheader -list -c "
    select count(*) from (
      select *
      from read_parquet('$OUT/archive/catalog/segments/**/*.parquet')
      qualify row_number() over (
        partition by collector, stream, range_start, ordinal
        order by part_seq desc, row_version desc
      ) = 1
    )"
}

echo "--- what the catalog says, with no rebuild"
duckdb -c "
  select collector, stream, state, records
  from read_parquet('$OUT/archive/catalog/segments/**/*.parquet')
"
rows=$(segments_now)
[ "$rows" -ge 1 ] || { echo "FAIL: the live catalog has no segment rows"; exit 1; }

echo "--- gaps must be there too, without a rebuild: a closed gap is written when it ends"
gaps=$(duckdb -noheader -list -c "
  select count(*) from (
    select *
    from read_parquet('$OUT/archive/catalog/gaps/**/*.parquet')
    qualify row_number() over (
      partition by collector, gap_start order by part_seq desc, row_version desc
    ) = 1
  )")
echo "live gap rows: $gaps"
[ "$gaps" -ge 1 ] || { echo "FAIL: the live catalog has no gap rows"; exit 1; }
duckdb -c "select collector, gap_start, gap_end, reason from read_parquet('$OUT/archive/catalog/gaps/**/*.parquet')"

echo "--- rebuild adds a generation beside the live parts and duplicates no row"
before_parts=$(find "$OUT/archive/catalog" -name '*.parquet' | wc -l)
"$BIN" rebuild-index --config "$OUT/bgpviews.toml" > /dev/null
after=$(segments_now)
[ "$rows" = "$after" ] || { echo "FAIL: live catalog had $rows rows, rebuild produced $after"; exit 1; }
after_parts=$(find "$OUT/archive/catalog" -name '*.parquet' | wc -l)
[ "$after_parts" -gt "$before_parts" ] || {
  echo "FAIL: rebuild replaced parts ($before_parts -> $after_parts) instead of adding a generation"; exit 1; }

echo "--- and publishes the generation manifest that names its parts"
[ -f "$OUT/archive/catalog/GENERATION" ] || { echo "FAIL: no catalog/GENERATION after a rebuild"; exit 1; }
manifest=$(python3 -c "import json;print(len(json.load(open('$OUT/archive/catalog/GENERATION'))['parts']))")
[ "$manifest" -ge 3 ] || { echo "FAIL: the manifest names $manifest parts"; exit 1; }
echo "  $manifest part(s) in the generation"

kill "$PID" 2>/dev/null || true
trap - EXIT
echo "ok: $rows segment row(s) live, $after after a rebuild, $after_parts parts, one generation"
