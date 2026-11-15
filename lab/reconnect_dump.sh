#!/usr/bin/env bash
# Does reconnecting the BMP target force a fresh dump (and therefore an EoR)?
#
# This is the question that decides whether a daemon can *obtain* a completion
# signal instead of waiting for one the router never sends.
#
# Sequence: bring the lab up (BGP Established), attach listener A and let it capture,
# close A, attach listener B on the same port while the BGP session stays up, and
# count what B receives. If B gets the full table and an EoR, the collector can force
# a dump by reconnecting.
set -euo pipefail

cd "$(dirname "$0")"

WINDOW=${WINDOW:-8}
OUT=run-reconnect
rm -rf "$OUT"
mkdir -p "$OUT"

count() {
  python3 - "$1" <<'PY'
import struct, sys

raw = open(sys.argv[1], "rb").read()
off = frames = records = eor = 0
while off < len(raw):
    length = struct.unpack(">I", raw[off + 1:off + 5])[0]
    body = raw[off:off + length]
    frames += 1
    if body[5] == 0:
        records += 1
        bgp_len = struct.unpack(">H", body[6 + 42 + 16:6 + 42 + 18])[0]
        if bgp_len in (23, 29):
            eor += 1
    off += length
print(f"frames={frames} records={records} eor={eor}")
PY
}

./down.sh > /dev/null 2>&1 || true
./up.sh > "$OUT/up.log" 2>&1
echo "lab up, BGP Established"

python3 ./bmp-listen.py "$OUT/a.bin" "$WINDOW" 17901 172.31.250.12 > "$OUT/a.log" 2>&1 &
A=$!
wait "$A" || true
echo "listener A (first attach):  $(count "$OUT/a.bin")"

python3 ./bmp-listen.py "$OUT/b.bin" "$WINDOW" 17901 172.31.250.12 > "$OUT/b.log" 2>&1 &
B=$!
wait "$B" || true
echo "listener B (reconnect):     $(count "$OUT/b.bin")"

./down.sh > /dev/null 2>&1 || true
