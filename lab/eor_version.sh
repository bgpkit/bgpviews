#!/usr/bin/env bash
# Count EoR markers from a given FRR image, in both attachment orders.
#
#   ./eor_version.sh quay.io/frrouting/frr:10.7.1 frr-ipv4-only 0
#
# Arguments: image, config directory, EXPECT_IPV6 (1 or 0).
# Uses the Python listener so the count does not depend on bgpviews at all.
set -euo pipefail

cd "$(dirname "$0")"

IMAGE=${1:?image tag}
CONF_DIR=${2:?config directory}
IPV6=${3:-1}
WINDOW=${WINDOW:-15}
OUT=run-eor-versions
mkdir -p "$OUT"

count() {
  python3 - "$1" <<'PY'
import struct, sys

raw = open(sys.argv[1], "rb").read()
off = frames = records = eor = 0
flags = set()
while off < len(raw):
    length = struct.unpack(">I", raw[off + 1:off + 5])[0]
    body = raw[off:off + length]
    frames += 1
    if body[5] == 0:
        records += 1
        flag = body[7]
        bgp_len = struct.unpack(">H", body[6 + 42 + 16:6 + 42 + 18])[0]
        if bgp_len in (23, 29):
            eor += 1
            flags.add(flag)
    off += length
print(f"frames={frames} records={records} eor={eor} eor_flags={sorted(hex(f) for f in flags)}")
PY
}

run_case() {
  local order=$1
  local label=$2
  local log="$OUT/$label.listener.log"
  local up_log="$OUT/$label.up.log"
  ./down.sh > /dev/null 2>&1 || true

  if [ "$order" = first ]; then
    python3 ./bmp-listen.py "$OUT/$label.bin" "$WINDOW" 17901 172.31.250.12 > "$log" 2>&1 &
    LISTENER=$!
    sleep 2
    FRR_IMAGE="$IMAGE" FRR_CONF_DIR="$CONF_DIR" EXPECT_IPV6="$IPV6" ./up.sh > "$up_log" 2>&1
  else
    FRR_IMAGE="$IMAGE" FRR_CONF_DIR="$CONF_DIR" EXPECT_IPV6="$IPV6" ./up.sh > "$up_log" 2>&1
    python3 ./bmp-listen.py "$OUT/$label.bin" "$WINDOW" 17901 172.31.250.12 > "$log" 2>&1 &
    LISTENER=$!
  fi

  wait "$LISTENER" || true
  ./down.sh > /dev/null 2>&1 || true
  printf '  %-16s %s\n' "$order" "$(count "$OUT/$label.bin")"
}

echo "image $IMAGE, config $CONF_DIR, expect_ipv6=$IPV6"
run_case first "$(echo "$IMAGE" | tr '/:' '__')-first"
run_case after "$(echo "$IMAGE" | tr '/:' '__')-after"
