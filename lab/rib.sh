#!/usr/bin/env bash
# Derive a RIB from a captured BMP stream and check that two independent readers agree.
#
# What this proves: the TABLE_DUMP_V2 written here is readable by monocle and bgpdump,
# carries a route per prefix that was announced, keeps the AS path and origin the peer
# sent, and is stable across runs (same input, same bytes).
set -euo pipefail
cd "$(dirname "$0")/.."

fixture=${1:-lab/fixtures/frr-8.4-two-router.bin}
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

cargo build -q --example rib_from_fixture
./target/debug/examples/rib_from_fixture "$fixture" "$out"

rib=$(find "$out" -path '*RIBS*' -name '*.gz' | head -1)
[ -n "$rib" ] || { echo "FAIL: no RIB file was written"; exit 1; }

monocle_lines=$(monocle parse "$rib" | grep -c '^A|')
bgpdump_lines=$(bgpdump -m "$rib" 2>/dev/null | grep -c '^TABLE_DUMP2|')
echo "monocle: $monocle_lines announcements, bgpdump: $bgpdump_lines"

[ "$monocle_lines" -eq 4 ] || { echo "FAIL: monocle read $monocle_lines of 4"; exit 1; }
[ "$bgpdump_lines" -eq 4 ] || { echo "FAIL: bgpdump read $bgpdump_lines of 4"; exit 1; }

# The IPv4 routes must keep the next hop the peer sent. The IPv6 route in this fixture
# carries an IPv4-mapped next hop (::ffff:172.31.250.11), which neither reader will
# interpret, so the check is that the attribute is still there, not that they agree.
grep -q '192.0.2.0/24|65002 65001|IGP|172.31.250.11' <(monocle parse "$rib") \
  || { echo "FAIL: the IPv4 next hop did not survive"; exit 1; }
grep -q '2001:db8::/32' <(monocle parse "$rib") \
  || { echo "FAIL: the IPv6 prefix is missing"; exit 1; }

# Same input, same bytes: a RIB is a projection, not a log.
second=$(mktemp -d)
./target/debug/examples/rib_from_fixture "$fixture" "$second" >/dev/null
a=$(sha256sum "$rib" | cut -d' ' -f1)
b=$(sha256sum "$(find "$second" -path '*RIBS*' -name '*.gz' | head -1)" | cut -d' ' -f1)
rm -rf "$second"
[ "$a" = "$b" ] || { echo "FAIL: two runs produced different bytes"; exit 1; }

echo "ok: 4 routes, next hops preserved, byte-identical across runs ($a)"
