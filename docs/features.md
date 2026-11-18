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
| Backpressure: a stall is named, a long one closes the session | 13 | `./lab/backpressure.sh`: forced 120 ms frames mark the interval `backpressure` and the 1 s deadline closes it; a healthy run carries no such reason | done |
| Disk guards: warning skips RIBs, critical closes the session | 13 | `./lab/disk.sh`: forced 91% skips the RIB, forced 98% closes the session | done |
| RIB derivation: per-(epoch, view) trie, TableDumpV2 + sidecar | 10 | RIB at a cutoff parses in monocle; peers match the updates | done (see the two rows above for what is left) |
| Quarantine: tombstone first, then move, then nothing | 7 | `./lab/quarantine.sh`: corrupt a segment, withhold it, `verify` stays clean and reports 1 withheld | done |
| Crash recovery: close the dead writer's open segment, drop temp files, republish the segment it claimed | 11 | `./lab/crash.sh`: SIGKILL mid-session, restart, the segment is closed at the last proven frame, the claim is republished as partial from the journal, `verify` is clean | done |
| Republish a claimed but unpublished segment from its journal | 5.5, 11 | `./lab/crash.sh`: the rebuilt artifact keeps the claim's epoch and ordinal, its sidecar reads `state=partial` with `partial_reasons=[writer_restart]` and the claim's own `interval_start`/`interval_end`, and `verify` resolves the claim; `republish::tests` cover identity, a second run as a no-op, a frame the live path refuses, and a journal that is gone | done |
| `session.json` carries who connected and how it ended | 11 | `retention::tests`: the record written at open names the source and the start, a later refresh keeps facts it does not know, and the close adds the end time, reason and counters; `lab/run.sh` leaves `source 172.31.250.12, started_at, ended_at, end_reason session_ended, frames 9, records 4` | done |
| Retention: per-part journal expiry with a tombstone and `session.json` | 11 | `./lab/retention.sh`: a closed part is expired, its tombstone and sequence range survive, `verify` stays clean | done |
| Parquet catalog (segments, sessions, gaps, peers) | 8 | `./lab/catalog.sh`: DuckDB reads 2 segments / 2 sessions / 3 gaps / 4 peers from the Parquet files | done (rebuild only) |
| Catalog parts are `catalog/<table>/YYYY.MM/YYYYMMDD.NNNNNN.parquet`, day from the row's own event time, `part_seq` and `row_version` on every row | 8 | `catalog::tests`: a 2025 stamp lands under `2025.11/20251115.`, two collectors in one interval get two parts, a second rebuild adds parts and deletes none | done |
| `rebuild-index` publishes a `catalog/GENERATION` manifest and fails on a sidecar it cannot read | 8 | `catalog::tests`: the manifest lists the new parts; a broken sidecar fails the rebuild and leaves the manifest where it was | done |
| Archive lock: one writer per root | 5 | `./lab/lock.sh`: a second `run` and a `rebuild-index` on a held root exit 3 with `writer_conflict` naming the holder; read-only commands are unaffected; a claim left by SIGKILL is reported, never broken | done |
| Sidecar `feed` records the transport that delivered the records | 4, 7 | `bgp_feed::tests`: a `feed = "bgp"` client's sidecar says `bgp`, not `bmp` | done |
| `adj-in-post` view archives post-policy frames | 4 | `collector::tests`: an `adj-in-post` collector keeps the two post-policy records and counts the pre-policy ones | done |
| Torn coverage tail is cut at the last validating line | 11 | `recovery::tests`: a torn append is dropped, the rest is byte-identical, a log with nothing valid is reported and left alone | done |
| Catalog updated on every publication | 8 | `./lab/catalog_live.sh`: DuckDB reads the segment rows with no rebuild; `rebuild-index` compacts without duplicating | done (segments and peers) |
| Gaps in the live catalog | 8 | `./lab/catalog_live.sh`: a closed gap row is present before any rebuild | done |
| `status` (coverage, segments, reasons, baseline) | 12 | `bgpviews status` on the lab archive; values match the sidecars and the coverage log | done (no 30-day matrix yet) |
| `verify` (both-direction reconciliation) | 8, 12 | `./lab/verify.sh`: a deleted segment is reported as a dangling claim, a stray one as an orphan | done |
| `verify` segments, hashes, journal replay, coverage invariants | 7, 8 | clean archive exits 0; one flipped byte exits 1 with the segment named | done |
| `rebuild-index` | 8, 12 | `./lab/catalog.sh`: delete `catalog/`, rebuild, the logical row count matches and the part count went up | done |
| BGP feed (`feed = "bgp"`, one view, max_prefixes) | 4 | in-process peer completes OPEN/KEEPALIVE/UPDATE/EoR; 1 segment, 2 records, baseline `observed` | done |
| BGP feed validated against a real router | 4 | `./lab/duck.sh`: duck's throwaway BIRD 2.0.12 (AS 400644) establishes, both prefixes archived, baseline observed | done |
| MD5 on the BGP feed | 4, 14 | not in v1. Decided by the maintainer on 2026-11-15: the syscall needs `unsafe`, and a transport credential is not worth the exception in a first release. `docs/experiments.md` E12 records what was learned so 1.1 does not rediscover it | out of scope for v1 |
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
4. `verify` (segments, hashes, journal replay, coverage invariants) and `rebuild-index`
   (each needs the previous).
5. Transaction, quarantine, retention, crash recovery: the failure paths, done last
   because they need the publication path to be final first.

## Open rows (v0.1)

These are known, and none of them is claimed as done above:

- **`loc-rib` and `loc-rib-filtered` views.** `check` accepts them, but the collector routes
  only global Adj-RIB-In frames, so a Loc-RIB collector would archive pre-policy adjacency
  frames under a Loc-RIB name. Section 4 lists the views as supported. What is missing is the
  peer-type routing (RFC 9069 Local Route Monitoring) plus a capture to validate it against;
  no lab router here sends Loc-RIB frames.
- **`segments` column names.** Section 8 calls the interval bounds `interval_start` and
  `interval_end`; the Parquet schema still says `range_start` and `range_end`, so the
  published query in section 8 would not run as written.
- **ADD-PATH.** A RIB from an ADD-PATH peer is still decoded as classic NLRI; refusals are
  counted but the segment is published as `complete` instead of partial.
- **A republish reads its whole epoch journal into memory.** `republish` collects the frames
  of the dead session's epoch and then replays them, so the startup cost of a rebuild is the
  size of that session's journal. `journal::replay` already materializes one part at a time,
  so streaming the two passes would remove the ceiling.
- **Republishing happens when the next session starts, not when the process starts.** It runs
  in the same housekeeping block as recovery, before the session's first write but after the
  router connects, while section 11 says `run` reconciles before accepting sockets.
- **`./lab/sighup.sh` needs the router to stay up.** It failed for a day because the accept loop
  consumed the SIGHUP flag while a session was serving, so the signal never reached the session
  that could snapshot; that is fixed and the lab passes. The lab still skips (exit 77) when the
  router drops the session first: in this environment the Docker Hub FRR image's daemons restart
  in a loop, and FRR 10.7.1 and 9.1.0 cannot bring the lab's eBGP session up at all. A skip is not
  a pass.
- **A `feed = "bgp"` claim is reported, not rebuilt.** The journal of a BGP session records
  messages without the peer identity a BGP4MP envelope needs, so that repair is refused
  rather than guessed.

## Known gaps found while implementing

- focl's TABLE_DUMP_V2 encoder takes `SnapshotRoute { prefix: Ipv4Addr, .. }`, so it cannot carry an IPv6 RIB entry. Reusing it for the RIB projection means either extending focl or writing the encoder here; it is the reason the RIB step is not a thin wrapper.
