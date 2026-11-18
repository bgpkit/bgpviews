#!/usr/bin/env bash
# Bring up the BMP lab: one docker network, two FRR routers, an established
# eBGP session, and proof that r2 is exporting BMP to the host.
#
# Idempotent: an existing network and existing containers are reused, and the
# routers are restarted so the committed frr/*.conf files are re-applied.
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

NET=bgpviews-lab
SUBNET=172.31.250.0/24
R1=bgpviews-lab-r1
R2=bgpviews-lab-r2
R1_IP=172.31.250.11
R2_IP=172.31.250.12
GATEWAY=172.31.250.1
IMAGE=${FRR_IMAGE:-frrouting/frr:latest}
CONF_DIR=${FRR_CONF_DIR:-frr}
WAIT_SECONDS=${WAIT_SECONDS:-60}
START=$SECONDS

die() {
  echo "error: $*" >&2
  exit 1
}

command -v docker >/dev/null 2>&1 || die "docker is not installed"
docker info >/dev/null 2>&1 || die "cannot talk to the docker daemon"
command -v python3 >/dev/null 2>&1 || die "python3 is required on the host"

# --- image ---------------------------------------------------------------
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  echo "pulling $IMAGE"
  docker pull "$IMAGE" ||
    die "could not pull $IMAGE; set FRR_IMAGE to an available FRR image"
fi
echo "using image $IMAGE"

# --- network -------------------------------------------------------------
if docker network inspect "$NET" >/dev/null 2>&1; then
  actual=$(docker network inspect -f '{{range .IPAM.Config}}{{.Subnet}}{{end}}' "$NET")
  if [ "$actual" != "$SUBNET" ]; then
    die "network $NET exists with subnet '$actual', expected $SUBNET; run ./down.sh first or remove that network"
  fi
  echo "network $NET already exists ($SUBNET)"
else
  docker network create --driver bridge --subnet "$SUBNET" "$NET" >/dev/null
  echo "created network $NET ($SUBNET)"
fi

# --- containers ----------------------------------------------------------
start_router() {
  local name=$1 host=$2 ip=$3 conf=$4
  local exists=0

  if docker container inspect "$name" >/dev/null 2>&1; then
    exists=1
    local cur_image cur_ip
    cur_image=$(docker inspect -f '{{.Config.Image}}' "$name")
    cur_ip=$(docker inspect -f '{{with index .NetworkSettings.Networks "bgpviews-lab"}}{{.IPAddress}}{{end}}' "$name")
    if [ "$cur_image" != "$IMAGE" ] || [ "$cur_ip" != "$ip" ]; then
      echo "recreating $name (image '$cur_image' -> '$IMAGE', address '$cur_ip' -> '$ip')"
      docker rm -f "$name" >/dev/null
      exists=0
    fi
  fi

  if [ "$exists" = 1 ]; then
    docker restart "$name" >/dev/null
    echo "restarted $name"
  else
    docker run -d \
      --name "$name" \
      --hostname "$host" \
      --network "$NET" --ip "$ip" \
      --ulimit nofile=1024:1024 \
      --cap-add NET_ADMIN --cap-add NET_RAW --cap-add SYS_ADMIN \
      -v "$PWD/frr/daemons:/etc/frr/daemons:ro" \
      -v "$PWD/$CONF_DIR/vtysh.conf:/etc/frr/vtysh.conf:ro" \
      -v "$PWD/$CONF_DIR/$conf:/etc/frr/frr.conf:ro" \
      "$IMAGE" >/dev/null
    echo "started $name ($IMAGE, $ip)"
  fi
}

start_router "$R1" r1 "$R1_IP" r1.conf
start_router "$R2" r2 "$R2_IP" r2.conf

# --- wait ----------------------------------------------------------------
bgp_established() {
  docker exec "$R1" vtysh -c "show bgp neighbors $R2_IP" 2>/dev/null |
    grep -q "BGP state = Established" &&
    docker exec "$R2" vtysh -c "show bgp neighbors $R1_IP" 2>/dev/null |
      grep -q "BGP state = Established"
}

bmp_target_visible() {
  docker exec "$R2" vtysh -c "show bmp" 2>/dev/null |
    grep -q 'Targets "bgpviews"'
}

routes_present() {
  local p
  for p in 192.0.2.0/24 198.51.100.0/24 203.0.113.0/24; do
    docker exec "$R2" vtysh -c "show bgp ipv4 unicast $p" 2>/dev/null |
      grep -q "BGP routing table entry" || return 1
  done
  if [ "${EXPECT_IPV6:-1}" = "1" ]; then
    docker exec "$R2" vtysh -c "show bgp ipv6 unicast 2001:db8::/32" 2>/dev/null |
      grep -q "BGP routing table entry" || return 1
  fi
  return 0
}

wait_for() {
  local desc=$1
  shift
  local deadline=$((SECONDS + WAIT_SECONDS))
  printf 'waiting for %s' "$desc"
  while [ "$SECONDS" -lt "$deadline" ]; do
    if "$@"; then
      printf ' (%.0fs)\n' "$((SECONDS - START))"
      return 0
    fi
    printf '.'
    sleep 1
  done
  printf ' (timeout after %ss)\n' "$WAIT_SECONDS"
  return 1
}

if ! wait_for "BGP session r1 <-> r2 Established" bgp_established; then
  echo "diagnostics:" >&2
  docker logs --tail 20 "$R1" >&2 || true
  docker logs --tail 20 "$R2" >&2 || true
  exit 1
fi

if ! bmp_target_visible; then
  echo "error: r2 has no BMP target \"bgpviews\"." >&2
  echo "The image must provide the bgpd_bmp module; check frr/daemons and the image tag." >&2
  docker exec "$R2" vtysh -c "show bmp" >&2 || true
  exit 1
fi

if ! wait_for "four test prefixes on r2" routes_present; then
  echo "diagnostics:" >&2
  docker exec "$R2" vtysh -c "show bgp ipv4 unicast" >&2 || true
  docker exec "$R2" vtysh -c "show bgp ipv6 unicast" >&2 || true
  exit 1
fi

echo
echo "lab is up in $((SECONDS - START))s"
echo "  $R1  $R1_IP  AS65001  announces 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24, 2001:db8::/32"
echo "  $R2  $R2_IP  AS65002  exports BMP pre-policy to $GATEWAY:17901"
echo "next: ./capture.sh    teardown: ./down.sh"
