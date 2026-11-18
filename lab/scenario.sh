#!/usr/bin/env bash
# Run one named lab scenario and keep its capture as a committed fixture.
#
#   ./scenario.sh churn      r1 withdraws two prefixes mid-capture
#   ./scenario.sh restart    r2's BGP session to r1 drops and comes back mid-capture
#   ./scenario.sh baseline   no change: the initial dump only
#
# Each scenario brings the lab up, starts the capture, applies the scenario, lets
# the capture window close, then tears the lab down. The fixture is named after the
# scenario so the test suite can replay it: fixtures/<scenario>.bin.
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

SCENARIO=${1:-baseline}
SECONDS_WINDOW=${SECONDS_WINDOW:-20}
ACTION_AT=${ACTION_AT:-5}
R1=bgpviews-lab-r1
R2=bgpviews-lab-r2
OUT="fixtures/${SCENARIO}.bin"

case "$SCENARIO" in
  baseline|churn|restart) ;;
  *) echo "error: unknown scenario '$SCENARIO' (baseline|churn|restart)" >&2; exit 2 ;;
esac

cleanup() { ./down.sh >/dev/null 2>&1 || true; }
trap cleanup EXIT

./up.sh

echo "scenario $SCENARIO: capturing ${SECONDS_WINDOW}s, acting at ${ACTION_AT}s"
./capture.sh "$SECONDS_WINDOW" "$OUT" &
CAPTURE_PID=$!

# Give the capture listener time to accept the connection and the initial dump to
# arrive, so the scenario's own messages are visibly later in the stream.
sleep "$ACTION_AT"

case "$SCENARIO" in
  churn)
    echo "withdrawing 198.51.100.0/24 and 203.0.113.0/24 on r1"
    docker exec "$R1" vtysh -c "configure terminal" \
      -c "router bgp 65001" -c "address-family ipv4 unicast" \
      -c "no network 198.51.100.0/24" -c "no network 203.0.113.0/24" >/dev/null
    ;;
  restart)
    echo "clearing the session on r2"
    docker exec "$R2" vtysh -c "clear bgp 172.31.250.11" >/dev/null
    ;;
  baseline)
    echo "no action: baseline capture"
    ;;
esac

wait "$CAPTURE_PID"

BYTES=$(wc -c < "$OUT")
echo "fixture $OUT: ${BYTES} bytes"
python3 ./bmp-inspect.py "$OUT"
