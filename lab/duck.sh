#!/usr/bin/env bash
# Validate the BGP feed against duck: a real BIRD, a real network path, no host changes.
#
# duck's own BIRD service is disabled. Starting it would bring up its Vultr sessions and
# start announcing BGPKIT prefixes, so this runs a throwaway BIRD instance on duck, as a
# transient systemd unit with its own config and control socket, peering over the tailnet to
# bgpviews on this host. Nothing in /etc/bird changes, and the unit is stopped at the end.
#
# Three things about the harness are not obvious and each cost real time to find:
#   * BIRD needs a `protocol device` block, or its BGP protocol sits in `start` forever.
#   * The tailnet peer is not on a connected subnet, so the session needs `multihop`.
#   * We listen on 1179, so the neighbour needs `port 1179`; BIRD defaults to 179.
#
# What it proves that the FRR lab cannot: BIRD 2.0.12 with a four-octet AS interoperates
# with the feed, the peer's own EoR arrives (so baseline is `observed`), and both families
# are archived.
set -euo pipefail
cd "$(dirname "$0")"

DUCK=${DUCK:-duck}
DUCK_TS=${DUCK_TS:-100.83.111.25}
US_TS=${US_TS:-100.96.35.72}
OUR_AS=${OUR_AS:-65001}
DUCK_AS=${DUCK_AS:-400644}
PORT=${PORT:-1179}
OUT=run-duck
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT/archive"

cat > "$OUT/bgpviews.toml" <<TOML
[archive]
root = "run-duck/archive"
journal_retention_days = 30

[[clients]]
name = "duck"
listen = "${US_TS}:${PORT}"
allow_from = ["${DUCK_TS}/32"]
feed = "bgp"
asn = ${OUR_AS}
max_prefixes = 1000

[clients.view]
name = "adj-in-pre"
updates_interval_minutes = 60
rib = true
TOML

echo "--- throwaway BIRD config on duck"
ssh "$DUCK" "sudo -n tee /tmp/bgpviews-peer.conf > /dev/null" <<BIRD
log stderr all;
router id 149.28.81.209;

protocol static owned4 {
    ipv4;
    route 192.100.82.0/24 blackhole;
}

protocol static owned6 {
    ipv6;
    route 2620:aa:a000::/48 blackhole;
}

protocol bgp bgpviews {
    local ${DUCK_TS} as ${DUCK_AS};
    neighbor ${US_TS} as ${OUR_AS} port ${PORT};
    multihop;
    ipv4 {
        import none;
        export where proto = "owned4";
    };
    ipv6 {
        import none;
        export where proto = "owned6";
    };
}

protocol device {
}
BIRD

cleanup () {
  kill "${PID:-0}" 2>/dev/null || true
  ssh -n "$DUCK" 'sudo -n systemctl stop bgpviews-peer 2>/dev/null; sudo -n systemctl reset-failed bgpviews-peer 2>/dev/null; sudo -n rm -f /tmp/bgpviews-peer.conf /tmp/bgpviews-peer.ctl /tmp/bgpviews-peer.pid; true' || true
}
trap cleanup EXIT

echo "--- bgpviews listening"
"$BIN" run --once --config "$OUT/bgpviews.toml" > "$OUT/duck.log" 2>&1 &
PID=$!
for _ in $(seq 1 60); do grep -q listening "$OUT/duck.log" 2>/dev/null && break; sleep 0.2; done
grep -q listening "$OUT/duck.log" || { echo "collector did not start"; cat "$OUT/duck.log"; exit 1; }

echo "--- BIRD starts and connects"
ssh -n "$DUCK" "sudo -n systemd-run --unit=bgpviews-peer --collect --property=Type=simple \
  /usr/sbin/bird -f -c /tmp/bgpviews-peer.conf -s /tmp/bgpviews-peer.ctl -P /tmp/bgpviews-peer.pid" > /dev/null
sleep 12

echo "--- what BIRD says"
ssh -n "$DUCK" 'sudo -n birdc -s /tmp/bgpviews-peer.ctl show protocols all bgpviews' | sed -n '1,22p'

sleep 8
echo "--- stopping BIRD: an administrative shutdown is a normal end, not a failure"
ssh -n "$DUCK" 'sudo -n systemctl stop bgpviews-peer' || true
wait "$PID" || true

echo "--- what the collector archived"
cat "$OUT/duck.log"

echo "--- read back with monocle: the updates"
monocle parse "$OUT"/archive/*/bgpdata/*/UPDATES/*.gz
echo "--- and the RIB at the boundary"
monocle parse "$OUT"/archive/*/bgpdata/*/RIBS/*.gz || true

echo "--- status"
"$BIN" status --config "$OUT/bgpviews.toml"
echo "--- verify"
"$BIN" verify --config "$OUT/bgpviews.toml"

echo "ok: duck's BIRD established, both families archived, baseline observed"
