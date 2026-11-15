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
OUT="fixtures/bmp-${STAMP}.bin"
START=$SECONDS

echo "capturing BMP for ${CAPTURE_SECONDS}s on 0.0.0.0:${PORT} -> ${OUT}"
python3 - "$OUT" "$CAPTURE_SECONDS" "$PORT" "$EXPECTED_SOURCE" <<'PY'
import socket
import sys
import time

path = sys.argv[1]
seconds = float(sys.argv[2])
port = int(sys.argv[3])
expected = sys.argv[4]

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
try:
    srv.bind(("0.0.0.0", port))
except OSError as exc:
    print(f"error: cannot bind 0.0.0.0:{port}: {exc}", file=sys.stderr)
    sys.exit(1)
srv.listen(4)
print(
    f"listening on 0.0.0.0:{port}, expecting BMP from {expected or 'any source'}",
    flush=True,
)

# One connection, with a bounded wait for the exporter's retry timer.
deadline = time.monotonic() + 60.0
conn = None
peer = None
while time.monotonic() < deadline:
    srv.settimeout(max(0.1, deadline - time.monotonic()))
    try:
        candidate, addr = srv.accept()
    except socket.timeout:
        break
    if expected and addr[0] != expected:
        print(f"rejected connection from {addr[0]}:{addr[1]} (not {expected})", flush=True)
        candidate.close()
        continue
    conn, peer = candidate, addr
    break

if conn is None:
    srv.close()
    print(
        "error: no BMP connection before the 60s accept timeout; run ./up.sh first",
        file=sys.stderr,
    )
    sys.exit(1)

print(f"accepted BMP connection from {peer[0]}:{peer[1]}", flush=True)
start = time.monotonic()
end = start + seconds
total = 0

try:
    with open(path, "wb") as fh:
        while True:
            remaining = end - time.monotonic()
            if remaining <= 0:
                print(f"stopped: {seconds:g}s capture window elapsed", flush=True)
                break
            conn.settimeout(min(0.5, remaining))
            try:
                chunk = conn.recv(65536)
            except socket.timeout:
                continue
            if not chunk:
                print("stopped: producer disconnected", flush=True)
                break
            fh.write(chunk)
            total += len(chunk)
finally:
    conn.close()
    srv.close()

elapsed = time.monotonic() - start
print(f"captured_bytes={total} elapsed_seconds={elapsed:.1f} path={path}", flush=True)
if total == 0:
    print("error: the BMP connection produced no bytes", file=sys.stderr)
    sys.exit(1)
PY

python3 bmp-inspect.py "$OUT" --require 0,3,4

SIZE=$(wc -c <"$OUT")
echo "capture_file=$OUT bytes=$SIZE wall_seconds=$((SECONDS - START))"
