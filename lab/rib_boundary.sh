#!/usr/bin/env bash
# The collector's own path writes a RIB at each interval boundary, and a reader can use it.
#
# This is the boundary half of the RIB contract. The SIGHUP half is separate: it must
# produce the same file at an arbitrary moment, not at a boundary. That is not
# implemented, so it is not asserted here.
#
# The evidence is `crates/bgpviews-core/tests/rib_boundary.rs`: two passes of the same
# fixture 900 s apart close one interval, and the snapshot must carry the prefixes of
# both families and be named after the boundary.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo test -q -p bgpviews-core --test rib_boundary -- --nocapture

echo "ok: a closed interval leaves a RIB under RIBS/, readable by bgpkit-parser"
