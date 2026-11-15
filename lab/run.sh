#!/usr/bin/env bash
# One-command reproduction: bring the lab up, capture and verify one BMP
# session, then tear the lab down again. Set KEEP=1 to leave it running.
set -euo pipefail

cd "$(dirname "$0")"

KEEP=${KEEP:-0}
cleanup() {
  if [ "$KEEP" != "1" ]; then
    ./down.sh
  fi
}
trap cleanup EXIT

./up.sh
./capture.sh "$@"
