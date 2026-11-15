#!/usr/bin/env bash
# Tear down only what this lab created: two named containers and the network.
set -euo pipefail

cd "$(dirname "$0")"

NET=bgpviews-lab

command -v docker >/dev/null 2>&1 || {
  echo "error: docker is not installed" >&2
  exit 1
}

for c in bgpviews-lab-r1 bgpviews-lab-r2; do
  if docker container inspect "$c" >/dev/null 2>&1; then
    docker rm -f "$c" >/dev/null
    echo "removed container $c"
  else
    echo "container $c not present"
  fi
done

if docker network inspect "$NET" >/dev/null 2>&1; then
  if docker network rm "$NET" >/dev/null; then
    echo "removed network $NET"
  else
    echo "error: could not remove network $NET (is another container attached?)" >&2
    exit 1
  fi
else
  echo "network $NET not present"
fi
