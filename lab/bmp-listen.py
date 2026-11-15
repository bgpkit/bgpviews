#!/usr/bin/env python3
"""Accept one BMP connection and write the raw byte stream to a file.

Used by capture.sh and by the EoR control experiment. It deliberately parses
nothing: it is the second implementation that keeps the Rust collector honest about
what the bytes were.

Usage: bmp-listen.py OUTPUT SECONDS PORT [EXPECTED_SOURCE]
"""
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
