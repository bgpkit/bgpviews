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
| `run` as a daemon: keep listening, replace a connection, new epoch per session | 12, 6 | kill the router session twice; two epochs, two segments, one coverage log | todo |
| SIGHUP reload and SIGHUP-triggered RIB | 11, 12 | `kill -HUP`; config re-read, RIB written at the next sequence boundary | todo |
| Backpressure: bounded queues, `backpressure` reason, session close | 13 | slow reader; interval marked `backpressure`, session closed after deadline | todo |
| Disk guards: `disk_warning` skips RIBs, `disk_critical` closes | 13 | fill a small filesystem; RIB `missed/disk_warning`, then session close | todo |
| RIB derivation: per-(epoch, view) trie, TableDumpV2 + sidecar | 10 | RIB at a cutoff parses in monocle; peers match the updates | todo |
| Segment transaction: work dir, hash, quarantine, `withheld` | 7 | fault injection before publication; `withheld` + quarantine + coverage range | todo |
| Crash recovery: reconcile transactions, torn journal, watermark | 7 | kill -9 at each publication step; one defined outcome per step | todo |
| Retention and tombstones, per-part journal expiry | 7 | expire one journal part; verify accepts the tombstone pair | todo |
| Parquet catalog (segments, sessions, peer_state, gaps, peers) | 8 | DuckDB query returns every segment once; `rebuild-index` reproduces it | todo |
| `status` (coverage, sessions, gaps, baseline) | 12 | prints the 30-day matrix and per-peer baseline from the archive alone | todo |
| `verify` (both-direction reconciliation) | 8, 12 | delete/hash-flip a file; `verify` names it with the right class | todo |
| `rebuild-index` | 8, 12 | delete the catalog; rebuild matches the pre-delete rows | todo |
| BGP feed (`feed = "bgp"`, one view, max_prefixes, MD5) | 4 | BIRD peer on duck; archive matches its Adj-RIB-In, EoR gives baseline `observed` | todo |
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
4. RIB derivation with the snap index, then the catalog, then `verify` and
   `rebuild-index` (each needs the previous).
5. Transaction, quarantine, retention, crash recovery: the failure paths, done last
   because they need the publication path to be final first.
