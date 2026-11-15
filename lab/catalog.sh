#!/usr/bin/env bash
# The catalog is a projection: rebuild it from published files, then query it with DuckDB.
#
# The point is not that Parquet exists. It is that a reader who was never told how the
# archive works can answer "what does this archive contain, and where are the holes" with
# one SQL statement, and that deleting the catalog costs time rather than data.
set -euo pipefail
cd "$(dirname "$0")"

OUT=run-catalog
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT"

# Use the existing two-session daemon archive if it is there, otherwise make one.
ARCHIVE="$OUT/archive"
if [ ! -d run-daemon/archive ]; then
  echo "creating an archive with two sessions"
  ./daemon.sh > /dev/null 2>&1 || { echo "SKIP: lab/daemon.sh needs docker"; exit 1; }
fi
cp -r run-daemon/archive "$ARCHIVE"

cat > "$OUT/bgpviews.toml" <<TOML
[archive]
root = "$ARCHIVE"

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

echo "--- rebuild-index"
"$BIN" rebuild-index --config "$OUT/bgpviews.toml"

echo "--- duckdb reads it back"
duckdb -c "
  select 'segments' as table_name, count(*) as rows from read_parquet('$ARCHIVE/catalog/segments/**/*.parquet')
  union all select 'sessions', count(*) from read_parquet('$ARCHIVE/catalog/sessions/**/*.parquet')
  union all select 'gaps', count(*) from read_parquet('$ARCHIVE/catalog/gaps/**/*.parquet')
  union all select 'peers', count(*) from read_parquet('$ARCHIVE/catalog/peers/**/*.parquet')
"

echo "--- the question the catalog exists to answer"
duckdb -c "
  select stream, state, count(*) as segments, sum(records) as records
  from read_parquet('$ARCHIVE/catalog/segments/**/*.parquet')
  group by stream, state order by stream, state
"

echo "--- gaps, including the open one (empty end)"
duckdb -c "
  select collector, gap_start, gap_end, reason
  from read_parquet('$ARCHIVE/catalog/gaps/**/*.parquet')
  order by gap_start limit 8
"

segments=$(duckdb -noheader -list -c "select count(*) from read_parquet('$ARCHIVE/catalog/segments/**/*.parquet')")
[ "$segments" -ge 2 ] || { echo "FAIL: expected at least two segment rows, got $segments"; exit 1; }

echo "--- deleting the catalog and rebuilding must reproduce it"
before=$(duckdb -noheader -list -c "select count(*) from read_parquet('$ARCHIVE/catalog/segments/**/*.parquet')")
rm -rf "$ARCHIVE/catalog"
"$BIN" rebuild-index --config "$OUT/bgpviews.toml" > /dev/null
after=$(duckdb -noheader -list -c "select count(*) from read_parquet('$ARCHIVE/catalog/segments/**/*.parquet')")
[ "$before" = "$after" ] || { echo "FAIL: rebuild gave $after rows, first build gave $before"; exit 1; }

echo "ok: $after segment rows, rebuilt from the sidecars alone"
