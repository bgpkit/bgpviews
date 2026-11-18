# bgpviews v1: designed features against the design

Source of truth: `docs/design.md` (v1.0-rc1, with the EoR amendment) and
`docs/experiments.md`. This file is the implementation checklist; each item names the
design section it comes from and the check that proves it. Status values: `done`,
`partial`, `todo`.

## Already there

| Feature | Design | Proof | Status |
|---|---|---|---|
| BMP framing and classification | 4 | `bmp::tests` (10-frame fixture) | done |
| `catalog/peer_state/`: one row per peer-state version, from the session logs | 8 | `./lab/catalog.sh` lists five tables and `peer_state` carries 12 rows for two sessions, one row per version the logs hold; an archive with no logs writes no part instead of an empty one | done |
| `verify` reports a cut peer-state log instead of staying silent about it | 7, 8 | `verify::tests::a_cut_peer_state_log_is_a_finding_not_a_silence`: a whole log reconciles with no finding, and the same log with a `peer_state.torn` beside it is reported. `./lab/verify.sh` still exits 0 on a clean archive | done |
| A torn `peer_state.log` is cut at its last whole frame, and the dropped bytes are kept | 6 | `peer_state::tests::a_torn_tail_keeps_the_versions_before_it` truncates the log, shows a reader reports the tear, then opens a writer: it cuts to the valid prefix, saves the suffix as `peer_state.torn`, and the next version is readable at 1, 2 | done |
| A version lost between its write and its fsync is rebuilt from the journal, and says so | 6 | `peer_state::tests::a_version_the_crash_lost_is_rebuilt_from_the_journal_and_marked`: a journal holding a Peer Down beside a log that stopped at its Peer Up yields rebuilt versions, every one carrying `reconstructed: true` while the live version does not; a second rebuild writes nothing. `main` runs it per claim before the segment repair | done |
| `peer_state.log`: one versioned payload per peer object, fsynced per event | 6 | `peer_state::tests` prove the version counter survives a restart (3, 4, not 1 again), a payload round-trips, a torn tail keeps the prefix. `collector::tests::the_peer_state_log_says_what_the_session_saw` feeds the lab capture and reads back four versions: IPv4 and IPv6 unicast, each a Peer Up version and an EoR version carrying `eor_received_at` and `eor_journal_seq` with the state still `synced`. `./lab/rotate.sh` leaves one version per real change: Peer Up 1 and Peer Down 2, no duplicate | done |
| Journal parts with per-frame CRC, batch fsync | 6 | `journal::tests`, replay equality | done |
| A journal frame says what it is, under the magic section 6 fixes | 6 | `journal::tests::the_part_names_itself_and_says_what_each_frame_is` writes one BMP and one BGP frame and finds `BGPVJNL1` plus kind 0 and kind 2 at the byte offsets section 6 gives; `a_part_written_under_the_old_magic_is_refused_by_name`; `republish::tests::a_bgp_feed_journal_is_refused_instead_of_rebuilt_as_bmp` keeps a BGP journal `Unresolved` with a reason naming kind 2 instead of turning BGP messages into a segment of nonsense | done |
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
| A republish reads the epoch one journal part at a time | 5.5, 11 | `republish::tests::a_session_whose_journal_rotated_is_still_rebuilt`: a live collector with a 400-byte part limit rotates the fixture's frames across several parts, the process dies, and the rebuild reads every part and keeps the records. The code holds one part (bounded by `journal_max_bytes`) instead of the whole epoch | done |
| A repair order survives a second crash | 11 | `recovery::tests::a_claim_survives_a_second_crash_because_it_is_written_down`: the claim is written to `sessions/<client>/<epoch>/repair.json` before the coverage gap replaces it, read back by the next recovery, and removed only once the segment is published again. `./lab/crash.sh` still republishes and verifies clean | done |
| A hole in the journal stops a rebuild instead of shortening it | 5.5, 11 | `republish::tests::a_hole_in_the_journal_stops_the_rebuild_instead_of_shortening_it`: parts 0 and 2 with 1 expired answer `Unresolved` naming the missing part; parts are also checked for sequence continuity across their boundary | done |
| A stranded artifact is removed only after its journal is found | 5.5, 11 | `republish::tests::a_stranded_artifact_survives_when_the_journal_is_gone`: with no journal for the claim's epoch the rebuild answers `Unresolved` and the sidecar-less artifact it would have replaced is still on disk | done |
| A claimed range with no frames produces a file that exists | 5.5, 11 | `republish::tests::an_empty_range_produces_a_file_that_exists`: `empty_segment` returns the path `finish` actually published, so the caller's read succeeds and the sidecar names the same file. Before, it returned the writer's original name and the read failed with NotFound on exactly the case the function exists for | done |
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
| `status` (coverage, segments, reasons, baseline) | 12 | `./lab/status.sh`: the collector, per-state segment counts, partial reasons and baseline lines are present; reading the archive leaves every journal part byte-identical; `status \| head -1` exits 0 with an empty stderr, because a reader that stops early is normal | done (no 30-day matrix yet) |
| `verify` (both-direction reconciliation) | 8, 12 | `./lab/verify.sh`: a deleted segment is reported as a dangling claim, a stray one as an orphan | done |
| Peer types 1 to 3 are routed to the view they belong to, or refused | 4 | `bmp::tests::a_loc_rib_instance_is_read_as_its_own_view_not_as_an_adjacency` pins `supported_peer` and the `F` flag; `FrameKind::PeerUp`/`RouteMonitoring` carry `supported_peer` and `loc_rib`; the collector's Peer Up arm refuses a frame whose type does not match the configured view (`PeerTypeForView`, segment marked `peer_type_for_view`) and ends the session. Types 1 and 2 stay refused as unsupported. **Not covered by a lab**: no router here sends type 3 frames, so the end-to-end path rests on the parse test plus `config` | partial |
| A RIB snapshot taken after a Peer Down does not carry that peer's paths | 4, 11 | `rib_boundary` integration test: `restart.bin` goes up, announces, comes down; the RIB is empty at the moment the Peer Down is applied, and the boundary snapshot is taken before the frame that closed the interval is applied, so a Peer Up that starts a new generation cannot erase the routes the closed interval summarizes | done |
| A Loc-RIB view is accepted only where it can be routed | 4 | `config::tests::a_loc_rib_view_validates_only_where_it_can_be_routed`: `loc-rib` and `loc-rib-filtered` validate for a `bmp` client without `rib = true`, and are refused on a `bgp` feed or with a RIB snapshot, because a Loc-RIB peer has no address to key a RIB by | done |
| `check` prints the snippet for the feed it was given | 15 | `lab/run.sh` and `lab/live.sh` consume the FRR `bmp targets` block and the BGP `neighbor` block; the vendor table in the README says which of the two exists and which of design section 17's four other vendors are **not built** | done |
| A sidecar reaches the disk before its directory entry does | 7 | `segment::finish` writes the sidecar through a file and syncs it before syncing the directory, so a power loss cannot leave a published MRT with an unreadable description; `./lab/verify.sh` and `./lab/crash.sh` still pass | done |
| A `feed = "bgp"` session keeps itself alive | 4 | `bgp_feed::tests::a_quiet_peer_is_told_we_are_here_until_its_hold_time_runs_out`: one interval of silence keeps the session, silence for the whole hold time ends it. The reader now wakes every 30 s and writes a KEEPALIVE; `./lab/live.sh` still passes | done |
| SIGHUP inside an interval writes a RIB | 9 | `./lab/sighup.sh` signals the instant the collector logs that it accepted the router's BMP session, and asserts a RIB appears without waiting for a boundary; three consecutive runs pass. The settle time is zero on purpose: in this environment r2 loses the peer within a second or two of its dump and the BMP session closes with it, so any delay is a race the lab loses. The RIB is written from memory and needs no records | done | `./lab/sighup.sh` waits for the router's BMP session to be accepted before it signals, then asserts a RIB appears without waiting for a boundary. Before the wait, the signal could arrive with no session open, which the collector correctly ignores; the lab read that as a failure and was flaky | done |
| A closed stdout is not a crash | 7 | `./lab/pipe.sh`: `status \| head -2` exits 0 with the reader's lines, and an unpiped run also exits 0. Every write goes through `out!`, which exits 0 on a broken pipe instead of panicking the way `println!` does | done |
| `verify` segments, hashes, journal replay, coverage invariants | 7, 8 | clean archive exits 0; one flipped byte exits 1 with the segment named | done |
| `rebuild-index` | 8, 12 | `./lab/catalog.sh`: delete `catalog/`, rebuild, the logical row count matches and the part count went up | done |
| BGP feed (`feed = "bgp"`, one view, max_prefixes) | 4 | in-process peer completes OPEN/KEEPALIVE/UPDATE/EoR; 1 segment, 2 records, baseline `observed` | done |
| BGP feed validated against a real router | 4 | `./lab/duck.sh`: duck's throwaway BIRD 2.0.12 (AS 400644) establishes, both prefixes archived, baseline observed | done |
| MD5 on the BGP feed | 4, 14 | not in v1. Decided by the maintainer on 2026-11-15: the syscall needs `unsafe`, and a transport credential is not worth the exception in a first release. `docs/experiments.md` E12 records what was learned so 1.1 does not rediscover it | out of scope for v1 |
| Multi-client, multi-view in one process | 12 | `./lab/multi.sh`: two clients accepted concurrently, both archives written, one clean verify | done |
| RPKI/ASPA fields | 15 (non-goal) | out of scope for v1 | n/a |
| `max_prefixes` closes the session instead of archiving a partial table | 4 | `bgp_feed::tests::max_prefixes_counts_the_table_not_the_history`: the ceiling counts the table the peer currently claims per AFI/SAFI, so a withdrawal brings it back under; exceeding it ends the session rather than writing a table as if it were the whole one. `./lab/live.sh` runs the feed | done |
| `verify` reconciles the catalog against the sidecars | 8 | `./lab/verify.sh` step 5 removes a sidecar and expects `catalog_stale`; the reverse comparison reports `catalog_missing`. A catalog is a projection, and one that disagrees with its source in either direction is otherwise discovered by trusting it | done |
| A torn `peer_state.log` is a finding, not a clean log | 8 | `./lab/verify.sh` step 4 cuts a log mid-frame and expects `ends inside a frame`; `verify` used to gate that finding on a `.torn` side file that only recovery writes, and `replay` returns zero versions for an unreadable log, which looked exactly like a fresh one | done |
| `catalog/GENERATION` follows a live append, not only a rebuild | 8 | `./lab/catalog_live.sh` compares the parts the manifest names with the parts on disk before any rebuild and expects them equal; a reader that follows the manifest used to miss every segment published since the last rebuild | done |
| An MRT with no sidecar is reported as an orphan | 8 | `./lab/verify.sh` step 3 removes a RIB's sidecar and expects `this MRT has no sidecar`; before this check `verify` built its set of present segments from sidecars alone, so such a file was invisible in both directions | done |
| The documentation is a checked deliverable | 18 | `./lab/docs.sh`: the changelog has an Unreleased section or a dated version heading, and site/index.html declares the same version as Cargo.toml, the README says how to install and links the design and the features table, its open rows still name the Loc-RIB and journal rows, and every `lab/<name>.sh` and design section the features table cites exists | done |
| A crash is repaired when the process starts, not when the router reconnects | 11 | `./lab/crash.sh` asserts the `recovered: closed segment` line appears **before** the `listening` line in the restarted process's log, so repair does not wait for a session. The pass is idempotent: a rebuilt claim is cleared, so the second run at session start finds nothing | done |
| An ADD-PATH peer is refused by name instead of being labelled from its capability | 4, 8 | `./lab/addpath.sh` turns RFC 7911 on in the FRR lab and asserts the session ends naming the families and that no record claims a path identifier. The capabilitity is read from the OPEN the router **received**; a first version trusted it, wrote subtype 9, and the segment then decoded to zero prefixes because FRR sends plain NLRI when it has one path per prefix. Which encoding a message uses cannot be decided from the session, so the session is refused | partial: the update stream is refused, not archived | `./lab/addpath.sh` then runs the same peer again under `archive.add_path = "detect"` and asserts the opposite pair of facts: the session is archived, every record keeps the plain subtype its bytes justify (type 17 subtype 4, never 8 or 9, because FRR sends plain NLRI), and all four announced prefixes read back through `examples/prefixes` | archived: nothing claims a path identifier, and the FRR peer never sends one |

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

- **`loc-rib` and `loc-rib-filtered` views, one instance per client.** Type 3 frames are now
  classified with their `F` flag, carry the family set their NLRI names, and are refused when
  they do not match the configured view. What is still missing is design section 4's matching
  across several instances: an exporter that sends more than one Loc-RIB instance to one
  client (for example the unfiltered and the filtered Loc-RIB of the same router) would put
  both into the same view, because the peer slot is keyed by address and a Loc-RIB address is
  zero-filled. The collector now records each instance's `(bgp_id, F)` and refuses a second,
  different instance that arrives while the first is up (`LocRibPeerAmbiguous`, segment marked
  `loc_rib_peer_ambiguous`; `collector::tests::a_second_loc_rib_instance_is_refused_while_the_first_is_up`).
  `capability_mismatch` is still not raised, because a client keeps one slot and no family
  containment test is needed while that holds.
  **Measured, so this is not retried blind:** the pinned `frrouting/frr:latest` is FRRouting
  8.4_git, and its `bmp monitor` offers only `ipv4`, `ipv6` and `l2vpn`; there is no `loc-rib`
  keyword, so this image cannot emit a type 3 frame. Newer tags were measured earlier and cannot
  bring the lab's eBGP session up here at all, which would make a Loc-RIB lab report an unrelated
  failure. A capture from a router that supports RFC 9069 is what would move this row.
- **No other gap is known.** The `segments` interval bounds are written as `interval_start`
  and `interval_end`, so the query published in section 8 runs as written; `./lab/catalog.sh`
  reads the parts back through DuckDB and `./lab/catalog_live.sh` checks the live rows.
- **ADD-PATH in a RIB.** The RIB derivation has no path identifier in its key. Under
  `add_path = "detect"` a message whose NLRI carries path identifiers is written to the update
  stream with subtype 8 or 9 but kept out of the snapshot, which is then published with
  `add_path_not_in_rib`; under `reject` the session never starts. So no snapshot ever silently
  keeps the last path it saw, and none represents two paths to one prefix.
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
