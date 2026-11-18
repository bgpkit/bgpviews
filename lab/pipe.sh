#!/usr/bin/env bash
# A reader that closes the pipe early is not a failure of this program.
#
# `bgpviews status` writes one line per segment and per reason, and an operator piping it into
# `head` or `less` and quitting is the normal case. `println!` panics when stdout is a pipe whose
# other end is gone, so the process exited 101 with a panic on stderr and a shell that treated it
# as a crash. This lab pins the fix: the writer must exit 0 with the lines the reader took.
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

BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

if [ ! -d run-catalog/archive ]; then
  ./catalog.sh > /dev/null 2>&1 || { echo "SKIP: lab/catalog.sh did not produce an archive"; exit 77; }
fi

set +e
"$BIN" status --config run-catalog/bgpviews.toml | head -2 > /dev/null
piped=${PIPESTATUS[0]}
"$BIN" status --config run-catalog/bgpviews.toml > /dev/null
whole=$?
set -e

if [ "$piped" -ne 0 ]; then
  echo "FAIL: status exited $piped after its reader closed the pipe"
  exit 1
fi
if [ "$whole" -ne 0 ]; then
  echo "FAIL: status exited $whole when nobody closed anything"
  exit 1
fi

echo "ok: status survives a closed pipe ($piped) and exits clean when it is not ($whole)"
