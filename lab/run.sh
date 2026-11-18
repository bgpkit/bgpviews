#!/usr/bin/env bash
# One-command reproduction: bring the lab up, capture and verify one BMP
# session, then tear the lab down again. Set KEEP=1 to leave it running.
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

KEEP=${KEEP:-0}
cleanup() {
  if [ "$KEEP" != "1" ]; then
    ./down.sh
  fi
}
trap cleanup EXIT

./up.sh
./capture.sh "$@"
