# BMP lab

This lab stands up two FRR routers on a user-defined Docker bridge, peers them
in AS65001 and AS65002, makes r2 export BMPv3 to a listener on the host, and
saves the raw byte stream as a test fixture. It exists because bgpviews' tests
replay captured BMP instead of needing a live router (`../docs/design.md`,
sections 14 and 16).

## What it proves

Measured on 2026-11-14 local time (UTC capture timestamps 2026-11-15T00:18Z) on
the host that built this lab, with `frrouting/frr:latest` (FRR 8.4_git):

- r1 and r2 reach `BGP state = Established` and r2 receives all four test
  prefixes: 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24, 2001:db8::/32.
- r2's BMP target connects to the host listener from 172.31.250.12 and sends a
  BMPv3 stream whose first byte is `0x03`.
- The stream walks cleanly to EOF using only the 6-byte BMP common header
  (version u8, length u32 big-endian including the header, type u8).
- A 15 s capture is 1076 bytes and contains 10 messages: 1 Initiation, 1 Peer
  Up, and 8 Route Monitoring. The Route Monitoring messages carry the three
  IPv4 prefixes and the IPv6 prefix as real UPDATEs, plus the four EoR markers
  FRR emits at the end of the initial dump. No trigger or route flap is needed:
  the dump happens when the BMP session is established.

## Prerequisites

- Linux host with Docker (tested with 26.1.5) and permission to create
  containers and networks.
- `python3` on the host, standard library only. It runs the capture listener
  and the independent capture reader.
- Host TCP port 17901 free.
- Docker subnet 172.31.250.0/24 free (the lab fails early if an existing
  `bgpviews-lab` network has a different subnet).
- The FRR image is pulled on first run. The default is
  `frrouting/frr:latest`, which resolved to FRR 8.4_git,
  `sha256:990e83490108b686fd6df3b1cafa6bdbb2714acb00eedb9a89693946f46f45ce`.
  Pin it with `FRR_IMAGE=frrouting/frr@sha256:990e...` if the tag moves.

## Run

One command, from a clean machine with Docker installed:

```sh
./lab/run.sh
```

`run.sh` runs `up.sh`, `capture.sh`, and `down.sh` in that order and leaves the
capture file behind. Set `KEEP=1` to leave the lab running:

```sh
KEEP=1 ./lab/run.sh
```

Manual sequence:

```sh
./lab/up.sh          # idempotent; waits for Established and the four routes
./lab/capture.sh     # 15 s default; prints the byte count and per-type counts
./lab/down.sh        # removes the two containers and the network only
./lab/capture.sh 30  # optional capture length in seconds
```

`up.sh` restarts existing lab containers so the committed `frr/*.conf` files
are re-applied. Do not run it during a capture.

## Teardown

```sh
./lab/down.sh
```

It removes exactly `bgpviews-lab-r1`, `bgpviews-lab-r2`, and the Docker network
`bgpviews-lab`. It does not prune images, volumes, or any other container or
network. Capture files in `fixtures/` are left in place.

## Output files

- `fixtures/bmp-<UTC timestamp>.bin`: the raw TCP payload of one BMP session,
  starting at the Initiation common header. It is a BMP stream, not a pcap, and
  it contains no TCP/IP framing.
- Nothing else is written. `fixtures/*.bin` is gitignored; `fixtures/.gitkeep`
  keeps the directory.

## Files

| Path | Purpose |
|---|---|
| `up.sh` | Creates or reuses the network, creates or restarts r1/r2, mounts the FRR configs, waits for Established and the four prefixes. |
| `capture.sh` | Binds `0.0.0.0:17901`, accepts one connection from 172.31.250.12, writes the bytes verbatim, stops after N seconds or on EOF, then runs the reader with `--require 0,3,4`. |
| `down.sh` | Removes only the two lab containers and the lab network. |
| `run.sh` | One-command wrapper: up, capture, verify, down; `KEEP=1` skips the down. |
| `bmp-inspect.py` | Independent BMPv3 common-header reader. Prints each message's version, type and length, per-type counts, and best-effort Initiation, per-peer and UPDATE NLRI detail. Exits 1 on a framing error or a missing required type. |
| `frr/r1.conf` | AS65001, announces the four prefixes. |
| `frr/r2.conf` | AS65002, peers with r1, exports BMP pre-policy for IPv4 and IPv6 unicast to 172.31.250.1:17901. |
| `frr/daemons` | Enables zebra and bgpd. Loads the `bgpd_bmp` module and bounds bgpd's fd table. |
| `frr/vtysh.conf` | `service integrated-vtysh-config`, avoids the missing-file warning at startup. |

## Verification commands

```sh
docker ps --filter name=bgpviews-lab
docker exec bgpviews-lab-r1 vtysh -c "show bgp summary"
docker exec bgpviews-lab-r2 vtysh -c "show bgp neighbors 172.31.250.11"
docker exec bgpviews-lab-r2 vtysh -c "show bmp"
python3 lab/bmp-inspect.py lab/fixtures/bmp-<timestamp>.bin --require 0,3,4
```

The measured run produced:

```text
file=lab/fixtures/bmp-20261115T001810Z.bin bytes=1076
first_byte=0x03
counts=0:8,3:1,4:1
required types present: 0,3,4
OK: BMPv3 stream walks cleanly to EOF
```

## Configuration notes that are easy to get wrong

- **BMP is a module in this image.** `bgpd` must be started with `-M bgpd_bmp`.
  Without it, `bmp targets` is an unknown command and no BMP leaves the router.
  `up.sh` fails with a clear message if `show bmp` has no target.
- **Pre-policy BMP needs the Adj-RIB-In.** FRR only keeps `bn->adj_in` when
  `neighbor ... soft-reconfiguration inbound` is configured. Without that line
  the capture contains EoRs and withdrawals but no announcements: the BMP code
  finds no `adj_in` attribute and encodes a withdraw. r2 sets it for both
  address families.
- **eBGP needs a policy.** With FRR's default eBGP policy requirement, the
  session establishes but `PfxSnt` and `PfxRcd` stay `(Policy)`. Both configs
  set `no bgp ebgp-requires-policy`.
- **The network statements need no RIB route.** `no bgp network import-check`
  lets r1 originate the four prefixes without installing matching routes.
- **The fd limit matters on this host.** The host's `ulimit -n` is about 1e9.
  bgpd preallocates roughly 64 bytes per possible fd, tries to allocate ~64 GiB,
  and fails to fork with `fork(): Out of memory`. The containers run with
  `--ulimit nofile=1024:1024` and bgpd gets `--limit-fds 1024`.
- **The capabilities are a minimum.** FRR 8.4's `privs_init` asks for
  `cap_net_admin`, `cap_net_raw`, and `cap_sys_admin`; the containers get those
  three, not `--privileged`.

## Findings and unverified assumptions

Measured facts:

- **FRR re-encodes the UPDATE for BMP; the capture is not a byte copy of the
  wire UPDATE.** The captured AS_PATH for all four prefixes is `65002 65001`
  while r2's BGP RIB path for the same prefix is `65001`. FRR's `bmp_update()`
  calls `bgp_packet_attribute()` with the monitored peer as the destination,
  which prepends the exporter's local AS. The capture is a genuine FRR BMP
  stream, but bgpviews must not assume a BMP Route Monitoring PDU is identical
  to the received wire PDU, and design acceptance test 1 cannot compare the
  captured AS path to r1's configured path without accounting for the extra
  local AS.
- **The IPv6 prefix travels with an IPv4-mapped next hop.** The MP_REACH next
  hop is `::ffff:172.31.250.11` because the BGP session is IPv4 and no IPv6
  address is configured on either router. The UPDATE is otherwise a normal IPv6
  announcement for 2001:db8::/32.
- **FRR emits EoR markers twice.** Even though r2 configures only pre-policy
  monitoring, each family's EoR arrives once with the L (post-policy) flag set
  and once clear. This is FRR's behavior, not a lab setting.
- **FRR 9.1.0 does not work with this configuration.** With
  `quay.io/frrouting/frr:9.1.0` the session stays Active/Idle and bgpd logs
  `No local IPv6 address, and zebra does not support V6 routing with v4
  nexthops, BGP routing for V6 will not work`. Docker leaves IPv6 disabled on
  the container interface (`disable_ipv6=1`, `/proc/sys` read-only), and adding
  an address inside the container fails. The lab is therefore tied to the 8.4
  behavior. Making it 9.1-compatible needs an IPv6-enabled Docker network (or
  an IPv6 address on `eth0`) or dropping the IPv6 AF.

Unverified assumptions:

- The capture relies on FRR sending the current Adj-RIB-In when the BMP session
  is established. It does so on 8.4_git with soft-reconfiguration enabled; a
  different FRR version may not, and then no trigger-free capture would exist.
- It assumes the Docker bridge gateway is `172.31.250.1` and that host firewall
  rules do not block container-to-host traffic on 17901. Both held here; a host
  firewall was not present, so that case is untested.
- The listener is a stand-in for bgpviews, not bgpviews. It enforces one
  expected source address and nothing else: no journal, epochs, MRT encoding,
  or coverage accounting.
- The cause of the AS_PATH mismatch is inferred from FRR's source. The measured
  fact is the mismatch between the BMP path and r2's RIB path; no packet capture
  of the wire UPDATE was taken to prove the wire path independently.
- `frrouting/frr:latest` may move. The digest above is what was tested.
