#!/usr/bin/env bash
# TCP-MD5 on the BGP feed, against duck's BIRD, with a password on both sides.
#
# This is the end-to-end proof that the key reaches the kernel and that the kernel enforces
# it: the collector installs a key for duck's address on its listener, BIRD signs with the
# same key and establishes, and a peer with the wrong key gets no session at all.
#
# duck's own bird.service stays disabled. The throwaway BIRD runs as a transient systemd
# unit over the tailnet and is stopped at the end.
set -euo pipefail
cd "$(dirname "$0")"

DUCK=${DUCK:-duck}
DUCK_TS=${DUCK_TS:-100.83.111.25}
US_TS=${US_TS:-100.96.35.72}
OUR_AS=${OUR_AS:-65001}
DUCK_AS=${DUCK_AS:-400644}
PORT=${PORT:-1180}
KEY=${KEY:-bgpviews-md5-test}
OUT=run-md5
BIN=../target/release/bgpviews
(cd .. && cargo build --release -q --bin bgpviews)

rm -rf "$OUT"
mkdir -p "$OUT/archive"

cat > "$OUT/bgpviews.toml" <<TOML
[archive]
root = "run-md5/archive"

[[clients]]
name = "duck"
listen = "${US_TS}:${PORT}"
allow_from = ["${DUCK_TS}/32"]
feed = "bgp"
asn = ${OUR_AS}
md5 = "${KEY}"
max_prefixes = 1000

[clients.view]
name = "adj-in-pre"
updates_interval_minutes = 60
rib = true
TOML

bird_config () { # $1 = password line (or empty)
  cat <<BIRD
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
    $1
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
}

start_bird () { # $1 = password line
  bird_config "$1" | timeout 20 ssh "$DUCK" "sudo -n tee /tmp/bgpviews-md5.conf > /dev/null"
  timeout 15 ssh -n "$DUCK" 'sudo -n systemctl stop bgpviews-md5 2>/dev/null; sudo -n systemctl reset-failed bgpviews-md5 2>/dev/null; true' || true
  ssh -n "$DUCK" "sudo -n systemd-run --unit=bgpviews-md5 --collect --property=Type=simple \
    /usr/sbin/bird -f -c /tmp/bgpviews-md5.conf -s /tmp/bgpviews-md5.ctl -P /tmp/bgpviews-md5.pid" > /dev/null
}

cleanup () {
  kill "${PID:-0}" 2>/dev/null || true
  timeout 15 ssh -n "$DUCK" 'sudo -n systemctl stop bgpviews-md5 2>/dev/null; sudo -n systemctl reset-failed bgpviews-md5 2>/dev/null; sudo -n rm -f /tmp/bgpviews-md5.conf /tmp/bgpviews-md5.ctl /tmp/bgpviews-md5.pid; true' || true
}
trap cleanup EXIT

echo "--- collector with a key for ${DUCK_TS}"
"$BIN" run --once --config "$OUT/bgpviews.toml" > "$OUT/with-key.log" 2>&1 &
PID=$!
for _ in $(seq 1 60); do grep -q listening "$OUT/with-key.log" 2>/dev/null && break; sleep 0.2; done
grep -q "requires TCP-MD5" "$OUT/with-key.log" \
  || { echo "FAIL: the collector did not install the key"; cat "$OUT/with-key.log"; exit 1; }

echo "--- BIRD signs with the same key"
start_bird "password \"${KEY}\";"
sleep 12
ssh -n "$DUCK" 'sudo -n birdc -s /tmp/bgpviews-md5.ctl show protocols bgpviews' | sed -n '1,6p'
established=$(ssh -n "$DUCK" 'sudo -n birdc -s /tmp/bgpviews-md5.ctl show protocols bgpviews' | grep -c Established || true)
echo "established: $established"

timeout 15 ssh -n "$DUCK" 'sudo -n systemctl stop bgpviews-md5 || true' || true
kill "$PID" 2>/dev/null || true

echo "--- what the collector archived"
cat "$OUT/with-key.log"
echo "--- read back"
monocle parse "$OUT"/archive/*/bgpdata/*/UPDATES/*.gz 2>/dev/null | head -4 || true
[ "$established" -ge 1 ] || { echo "FAIL: the MD5 session never established"; exit 1; }

echo "--- and a wrong key must not get a session"
rm -rf "$OUT/wrong" && mkdir -p "$OUT/wrong/archive"
sed "s|run-md5/archive|run-md5/wrong/archive|; s|:${PORT}|:$((PORT + 1))|" "$OUT/bgpviews.toml" > "$OUT/wrong/bgpviews.toml"
"$BIN" run --once --config "$OUT/wrong/bgpviews.toml" > "$OUT/wrong-key.log" 2>&1 &
PID=$!
for _ in $(seq 1 60); do grep -q listening "$OUT/wrong-key.log" 2>/dev/null && break; sleep 0.2; done
PORT=$((PORT + 1)) start_bird "password \"not-the-same-key\";"
sleep 12
ssh -n "$DUCK" 'sudo -n birdc -s /tmp/bgpviews-md5.ctl show protocols bgpviews' | sed -n '1,6p'
wrong=$(ssh -n "$DUCK" 'sudo -n birdc -s /tmp/bgpviews-md5.ctl show protocols bgpviews' | grep -c Established || true)
echo "established with the wrong key: $wrong"
timeout 15 ssh -n "$DUCK" 'sudo -n systemctl stop bgpviews-md5 || true' || true
kill "$PID" 2>/dev/null || true
[ "$wrong" -eq 0 ] || { echo "FAIL: a session established without the matching key"; exit 1; }

echo "ok: MD5 enforced, session with the right key archived, wrong key refused"
