# bgpviews v1: designed features against the design

Source of truth: `docs/design.md` (v1.0-rc1, with the EoR amendment) and
`docs/experiments.md`. This file is the implementation checklist; each item names the
design section it comes from and the check that proves it. Status values: `done`,
`partial`, `todo`.

## Already there

| Feature | Design | Proof | Status |
|---|---|---|---|
| BMP framing and classification | 4 | `bmp::tests` (10-frame fixture) | done |
| Journal parts with per-frame CRC, batch fsync | 6 | `journal::tests`, replay equality | done |
| Coverage timeline + file, invariants | 5.4, 5.5 | `coverage::tests`, replay tests | done |
| BGP4MP_ET updates segment, sidecar, sha256 | 5, 7 | `bgp4mp`, `segment`, monocle + bgpdump | done |
| Collector state machine (rollover, peer up/down, session end) | 5.4 | `collector::tests`, `lab_scenarios` | done |
| Replay harness over a capture | 16 | `tests/replay_fixture.rs` | done |
| `check` + config validation + FRR snippet | 11, 12 | `config::tests` | done |
| `run` serving one session, source allowlist | 12, 14 | `lab/live.sh` | done |
| Baseline evidence (first EoR per generation/family) | 5.5, 6 | `collector::tests`, live lab both orders | done |
| RIB index over bgpkit-snap | 10 | `rib_index::tests`, `rib_index_bench` | done |

## Designed, not implemented

| Feature | Design | Acceptance | Status |
|---|---|---|---|
| RIB projection: TABLE_DUMP_V2 with IPv4 and IPv6, attributes copied not rebuilt | 6, 7 | `./lab/rib.sh`: 4 routes, both readers agree, byte-identical across runs | done (fixture path) |
| RIB written at an interval boundary by the collector | 6 | `./lab/rib_boundary.sh`: two passes 900 s apart leave a RIB named after the boundary | done |
| RIB on SIGHUP, at an arbitrary moment | 9 | `kill -HUP` during a session; a RIB appears without waiting for the boundary | todo |
| `run` as a daemon: keep listening, new epoch per session | 12, 6 | `lab/daemon.sh`: two sessions, epochs 1 and 2, ordinals 1 and 2, one coverage log | done |
| Replace a live connection when the router redials | 12 | the second connection waits for the first session to end, then takes epoch 2 | partial |
| SIGHUP reload and SIGHUP-triggered RIB | 11, 12 | `kill -HUP`; config re-read, RIB written at the next sequence boundary | todo |
| Backpressure: bounded queues, `backpressure` reason, session close | 13 | slow reader; interval marked `backpressure`, session closed after deadline | todo |
| Disk guards: `disk_warning` skips RIBs, `disk_critical` closes | 13 | fill a small filesystem; RIB `missed/disk_warning`, then session close | todo |
| RIB derivation: per-(epoch, view) trie, TableDumpV2 + sidecar | 10 | RIB at a cutoff parses in monocle; peers match the updates | done (see the two rows above for what is left) |
| Segment transaction: work dir, hash, quarantine, `withheld` | 7 | fault injection before publication; `withheld` + quarantine + coverage range | todo |
| Crash recovery: reconcile transactions, torn journal, watermark | 7 | kill -9 at each publication step; one defined outcome per step | todo |
| Retention and tombstones, per-part journal expiry | 7 | expire one journal part; verify accepts the tombstone pair | todo |
| Parquet catalog (segments, sessions, peer_state, gaps, peers) | 8 | DuckDB query returns every segment once; `rebuild-index` reproduces it | todo |
| `status` (coverage, segments, reasons, baseline) | 12 | `bgpviews status` on the lab archive; values match the sidecars and the coverage log | done (no 30-day matrix yet) |
| `verify` (both-direction reconciliation) | 8, 12 | delete/hash-flip a file; `verify` names it with the right class | todo |
| `rebuild-index` | 8, 12 | delete the catalog; rebuild matches the pre-delete rows | todo |
| BGP feed (`feed = "bgp"`, one view, max_prefixes) | 4 | in-process peer test; duck still needs its import filter changed | partial |
| MD5 on the BGP feed | 4, 14 | not implemented: TCP_MD5SIG needs a socket option the feed does not set | todo |
| Multi-client, multi-view in one process | 11 | two clients, two views, four collectors, one process | todo |
| RPKI/ASPA fields | 15 (non-goal) | out of scope for v1 | n/a |

## Deployment targets

- **FRR lab on this host**: validates BMP. Working; the lab is the fixture generator
  and the EoR/baseline experiments live here.
- **duck** (`duck.barking-neon.ts.net`, Debian 12, 1 vCPU, 23 GB disk, BIRD 2.0.12):
  validates the **BGP feed**, because BIRD has no BMP support (`strings` finds no
  `bmp` in the binary). Two obstacles recorded:
  1. Both of duck's BGP sessions are `import none`, so an Adj-RIB-In would be empty.
     Validating a table requires changing each session's import filter to accept
     routes. That is a change to a router config and needs authorization.
  2. The BGP feed does not exist yet in code; it is the gating item for this target.

## Order of work

1. `run` daemon (epochs, reconnect, SIGHUP) because every later check runs under it.
2. BGP feed, because duck cannot be validated without it.
3. `status` next: it is read-only, it makes the rest observable, and it exercises the
   coverage log and sidecars from the reader's side.
| `verify` (segments, hashes, journal replay, coverage invariants) | 7, 8 | clean archive exits 0; one flipped byte exits 1 with the segment named | done |
   `rebuild-index` (each needs the previous).
5. Transaction, quarantine, retention, crash recovery: the failure paths, done last
   because they need the publication path to be final first.

## Known gaps found while implementing

- focl's TABLE_DUMP_V2 encoder takes `SnapshotRoute { prefix: Ipv4Addr, .. }`, so it cannot carry an IPv6 RIB entry. Reusing it for the RIB projection means either extending focl or writing the encoder here; it is the reason the RIB step is not a thin wrapper.
