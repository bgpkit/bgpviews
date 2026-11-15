#!/usr/bin/env python3
"""Hold a BMP connection open without sending anything.

Used by lab/replace.sh to occupy a client's session while the real router is brought up,
which is the only way to see a live session being replaced: a router that restarts drops its
own connection first, so the collector sees an end, not a replacement.

This is a test fixture, not a tool. It sends no bytes, so the collector sees a peer that
connected and then said nothing.
"""
import socket
import sys
import time

host, port, seconds = sys.argv[1], int(sys.argv[2]), float(sys.argv[3])
sock = socket.create_connection((host, port))
print(f"dummy connected to {host}:{port}", flush=True)
time.sleep(seconds)
sock.close()
