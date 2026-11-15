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
| RIB on SIGHUP, at an arbitrary moment | 9 | `./lab/sighup.sh`: 60-minute interval, `kill -HUP`, a RIB appears in under a second | done |
| Config reload on SIGHUP, identity-safe | 9 | `./lab/reload.sh`: rib applied live, listen/interval refused by name, a broken file keeps the running configuration | done |
| `run` as a daemon: keep listening, new epoch per session | 12, 6 | `lab/daemon.sh`: two sessions, epochs 1 and 2, ordinals 1 and 2, one coverage log | done |
| Replace a live connection when the router redials | 12 | `./lab/replace.sh`: a silent client holds the session, the router connects, the old session ends at a frame boundary and a second epoch appears | done |
| Backpressure: bounded queues, `backpressure` reason, session close | 13 | slow reader; interval marked `backpressure`, session closed after deadline | todo |
| Disk guards: warning skips RIBs, critical closes the session | 13 | `./lab/disk.sh`: forced 91% skips the RIB, forced 98% closes the session | done |
| RIB derivation: per-(epoch, view) trie, TableDumpV2 + sidecar | 10 | RIB at a cutoff parses in monocle; peers match the updates | done (see the two rows above for what is left) |
| Quarantine: tombstone first, then move, then nothing | 7 | `./lab/quarantine.sh`: corrupt a segment, withhold it, `verify` stays clean and reports 1 withheld | done |
| Crash recovery: close the dead writer's open segment, drop temp files | 11 | `./lab/crash.sh`: SIGKILL mid-session, restart, the segment is closed at the last proven frame, verify is clean | done |
| Retention: per-part journal expiry with a tombstone and `session.json` | 11 | `./lab/retention.sh`: a closed part is expired, its tombstone and sequence range survive, `verify` stays clean | done |
| Parquet catalog (segments, sessions, gaps, peers) | 8 | `./lab/catalog.sh`: DuckDB reads 2 segments / 2 sessions / 3 gaps / 4 peers from the Parquet files | done (rebuild only) |
| Catalog updated on every publication | 8 | `./lab/catalog_live.sh`: DuckDB reads the segment rows with no rebuild; `rebuild-index` compacts without duplicating | done (segments and peers) |
| Gaps in the live catalog | 8 | `./lab/catalog_live.sh`: a closed gap row is present before any rebuild | done |
| `status` (coverage, segments, reasons, baseline) | 12 | `bgpviews status` on the lab archive; values match the sidecars and the coverage log | done (no 30-day matrix yet) |
| `verify` (both-direction reconciliation) | 8, 12 | delete/hash-flip a file; `verify` names it with the right class | todo |
| `rebuild-index` | 8, 12 | `./lab/catalog.sh`: delete `catalog/`, rebuild, DuckDB row counts match | done |
| BGP feed (`feed = "bgp"`, one view, max_prefixes) | 4 | in-process peer completes OPEN/KEEPALIVE/UPDATE/EoR; 1 segment, 2 records, baseline `observed` | done |
| BGP feed validated against a real router | 4 | `./lab/duck.sh`: duck's throwaway BIRD 2.0.12 (AS 400644) establishes, both prefixes archived, baseline observed | done |
| MD5 on the BGP feed | 4, 14 | not implemented: TCP_MD5SIG needs a socket option the feed does not set | todo |
| Multi-client, multi-view in one process | 12 | `./lab/multi.sh`: two clients accepted concurrently, both archives written, one clean verify | done |
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
  2. The BGP feed exists and is tested in-process; what is missing is a live BIRD on duck
     that peers with it, which needs one configuration change there.

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
