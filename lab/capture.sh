#!/usr/bin/env bash
# Capture one raw BMPv3 session from r2 to a file.
#
# Binds a TCP listener on the host, accepts exactly one connection from r2,
# writes the bytes verbatim to fixtures/bmp-<UTC timestamp>.bin, and stops
# after CAPTURE_SECONDS (default 15) or when the producer disconnects.
#
# The listener also runs bmp-inspect.py on the result, requiring Initiation
# (4), Peer Up (3) and Route Monitoring (0), so a bad capture fails loudly.
set -euo pipefail

cd "$(dirname "$0")"

CAPTURE_SECONDS=${1:-${CAPTURE_SECONDS:-15}}
PORT=17901
EXPECTED_SOURCE=${EXPECTED_SOURCE:-172.31.250.12}

command -v python3 >/dev/null 2>&1 || {
  echo "error: python3 is required on the host" >&2
  exit 1
}

for c in bgpviews-lab-r1 bgpviews-lab-r2; do
  if ! docker container inspect "$c" >/dev/null 2>&1; then
    echo "error: container $c not found; run ./up.sh first" >&2
    exit 1
  fi
  if [ "$(docker inspect -f '{{.State.Running}}' "$c")" != "true" ]; then
    echo "error: container $c is not running; run ./up.sh" >&2
    exit 1
  fi
done

mkdir -p fixtures
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
# Second argument names the output file, which the scenario runner uses to keep
# one fixture per scenario instead of one per run.
OUT=${2:-"fixtures/bmp-${STAMP}.bin"}
START=$SECONDS

echo "capturing BMP for ${CAPTURE_SECONDS}s on 0.0.0.0:${PORT} -> ${OUT}"
python3 ./bmp-listen.py "$OUT" "$CAPTURE_SECONDS" "$PORT" "$EXPECTED_SOURCE"

python3 bmp-inspect.py "$OUT" --require 0,3,4

SIZE=$(wc -c <"$OUT")
echo "capture_file=$OUT bytes=$SIZE wall_seconds=$((SECONDS - START))"
