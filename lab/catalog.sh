#!/usr/bin/env bash
# The catalog is a projection: rebuild it from published files, then query it with DuckDB.
#
# The point is not that Parquet exists. It is that a reader who was never told how the
# archive works can answer "what does this archive contain, and where are the holes" with
# one SQL statement, and that deleting the catalog costs time rather than data.
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
# The committed lab archive carries a catalog written by an older binary, with the column names
# that version used. A rebuild adds parts and never deletes one, so a glob over the two
# generations would union two schemas; this lab is about rebuilding from the sidecars, so it
# starts from a clean projection.
rm -rf "$ARCHIVE/catalog"

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

# The published definition of current state: the row with the greatest part sequence per
# key. A rebuild adds parts, so a plain count would double after one.
segments_now() {
  duckdb -noheader -list -c "
    select count(*) from (
      select *
      from read_parquet('$ARCHIVE/catalog/segments/**/*.parquet')
      qualify row_number() over (
        partition by collector, stream, interval_start, ordinal
        order by part_seq desc, row_version desc
      ) = 1
    )"
}

segments=$(segments_now)
[ "$segments" -ge 2 ] || { echo "FAIL: expected at least two segment rows, got $segments"; exit 1; }

echo "--- deleting the catalog and rebuilding must reproduce it"
before=$(segments_now)
parts_before=$(find "$ARCHIVE/catalog" -name '*.parquet' | wc -l)
"$BIN" rebuild-index --config "$OUT/bgpviews.toml" > /dev/null
after=$(segments_now)
parts_after=$(find "$ARCHIVE/catalog" -name '*.parquet' | wc -l)
[ "$before" = "$after" ] || { echo "FAIL: rebuild gave $after rows, first build gave $before"; exit 1; }
[ "$parts_after" -gt "$parts_before" ] || {
  echo "FAIL: rebuild rewrote parts ($parts_before -> $parts_after) instead of adding a generation"; exit 1; }
[ -f "$ARCHIVE/catalog/GENERATION" ] || { echo "FAIL: no GENERATION manifest"; exit 1; }

echo "--- deleting every part and rebuilding from the sidecars alone must still reproduce it"
rm -rf "$ARCHIVE/catalog"
"$BIN" rebuild-index --config "$OUT/bgpviews.toml" > /dev/null
from_scratch=$(segments_now)
[ "$before" = "$from_scratch" ] || { echo "FAIL: a rebuild from nothing gave $from_scratch rows, the live catalog had $before"; exit 1; }

echo "ok: $from_scratch segment rows, rebuilt from the sidecars alone"
