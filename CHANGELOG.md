# Changelog

Notable changes, newest first. The newest heading is the newest tagged release; work that has
not been tagged yet lives under `## Unreleased` above it. The archive format is still allowed to
change between v0.x releases.

## Unreleased

- **`adj-out` is a view, not an exclusion.** A collector whose view is named `adj-out` records the
  frames an exporter marks with the RFC 8671 `O` flag, which is what a router (or a speaker such as
  `focld`) sends its peers; every other view counts those frames as `unconfigured_view`, exactly as
  before. Both directions of one session reach their own tree whether the sender uses one connection
  carrying both or one connection per direction. `collector::tests::an_adj_out_collector_takes_the_outbound_frames_and_counts_the_inbound_ones`
  proves the split, and `check` now says so instead of printing an FRR stanza that FRR cannot produce
  for this view.
- **Two collectors writing one catalog table no longer collide.** `write_part` read the published
  part numbers, and a part still sitting at its `.tmp` name is not one of them, so two clients of one
  archive (the ordinary case once an archive records more than one view) picked the same number,
  shared one tmp path, and the second rename failed with `ENOENT` or replaced rows the first writer
  had just published: `run` logged `catalog append failed: publish a parquet part`, and `verify` then
  reported the segment as `catalog_missing`. Part allocation now holds one lock across the whole
  write, which the archive's own single-writer-per-root lock makes sufficient.
  `catalog::tests::two_writers_racing_one_table_never_share_a_part_number` fails on the previous code
  with that same `ENOENT`.

## v0.1.5 - 2026-11-19

- **An empty segment now says which interval it covers.** A segment with no records carried a range
  that collapsed to the instant the writer opened at, so a 5-minute archive reported
  `range_start == range_end` for every silent interval. The coverage log was always right, but the
  sidecar and the catalog row projected from it said the interval was covered by nothing, which is
  the distinction this archive exists to keep: a reader who joined sidecar ranges instead of parsing
  the coverage log saw a hole where the writer had been watching and nothing happened. An empty
  segment now ends at its interval boundary, and `state` plus `partial_reasons` keep saying how much
  of the interval was actually observed. `collector::tests::a_silent_peer_publishes_each_interval_at_its_boundary`
  asserts the width of that range and fails on the previous expression.
- **Recovery publishes the segment a coverage line promised, even when the journal cannot prove it.**
  A kill can land with a coverage line naming a segment whose opening frame the journal never retained,
  and `republish` answered `Unresolved` for it: the claim then dangled forever, and `verify` reported
  `missing ... the coverage log names a segment with no sidecar` on every run for the rest of that
  archive's life. The rebuild now publishes what the retained frames prove, with `records=0` when they
  prove nothing, marked `partial` with `journal_corrupt`. `./lab/crash.sh` failed this way in about one
  run in three before the fix, on the code that predates it as well.
- **A coverage line is written only after the frames it covers are synced.** The line is fsynced when it
  is written, so it is a promise; the journal batches at 1 MiB by default, so that promise could outlive
  its evidence. `coverage_segment` now flushes the journal first.
- **Three labs state their RIB cadence.** `rib = true` no longer means a snapshot at every update
  boundary: it means a snapshot at every boundary that is a whole multiple of `rib_interval_minutes`,
  whose default is 480. `quiet.sh`, `verify.sh` and `status.sh` set the RIB interval equal to their
  update interval, which is what they were written to observe, so the labs keep testing snapshots
  rather than silently accepting zero.

## v0.1.4 - 2026-11-19

- **RIB snapshots are off by default, and they have their own cadence.** A view used to write a RIB at every update boundary, with the update interval as the only knob: at the 15-minute interval the quickstart showed, that is 96 full-table snapshots a day, about 85 MB each from the design's own measurement of a RouteViews RIB, so a first run against a full table filled a disk. Design section 14 always described `ribs = { enabled, interval, anchor = "utc" }`; the build had flattened that to one boolean on the update grid. The flat keys are now `rib` (default `false`), `rib_interval_minutes` (default `480`, the 8-hour cadence RIPE RIS publishes at) and the same UTC grid as before, so an 8-hour cadence lands at 00:00, 08:00 and 16:00 UTC. `check` refuses a RIB interval that is not a whole multiple of the update interval, because such a pair never comes due.
- **The update interval defaults to 5 minutes**, the cadence RIPE RIS publishes updates at, so a new archive lines up with the public archives without being told to. It was a required field before, which meant no default existed at all.
- **The README quickstart follows those defaults** and states the byte cost of turning snapshots on.

## v0.1.3 - 2026-11-18

- **The Linux release binaries are static.** They were built for `x86_64-unknown-linux-gnu` on `ubuntu-latest`, which carries that image's glibc floor: v0.1.2's binary demanded `GLIBC_2.38` and failed to start on Debian 12 (`2.36`) and Ubuntu 22.04 with `GLIBC_2.38 not found`, while the README advertised a linux x86_64 binary. Both Linux targets are now `*-unknown-linux-musl`, built natively on their own runner, and the build job refuses a dynamically linked result, so a binary that only runs on newer distributions cannot be published again.
- **A release is one transaction: it is created only after every platform has built.** The `Release` workflow created the GitHub release first and uploaded per platform afterwards, so a platform that failed to compile left a published release with a hole in it. `build-binaries` now builds all three targets into workflow artifacts, `create-release` runs only when all of them succeeded, and `publish-assets` attaches the artifacts that were already built rather than compiling again.
- **A tag also gates the image.** `Docker` runs the same formatting, clippy and test gates as `ci.yml` on a tag ref before publishing, because the two workflows are independent: v0.1.1's release refused to build while its images reached Docker Hub as `0.1.1`, `0.1` and `latest`.
- **`Release` can be rehearsed.** `workflow_dispatch` runs every gate and every platform build and creates nothing, so the workflow can be proven on `main` before a tag depends on it.

## v0.1.2 - 2026-11-18

- **This is the release v0.1.1 did not produce.** The v0.1.1 tag's `Release` workflow failed in `build-test` on `clippy::needless_borrows_for_generic_args`, a lint Rust 1.99's clippy added and the authoring host's 1.97 does not have, so its `create-release` and `upload-assets` jobs never ran: that tag carries no binaries, while its Docker images reached Docker Hub because the image workflow is independent. The lint is fixed here, and the tag stays where it is because a registry already holds its artifacts; tags are not moved once that is true. The archive behaviour is identical between the two.
## v0.1.1 - 2026-11-18

- **An interval closes at its own boundary, not when the next record arrives.** A collector now closes and publishes the interval the wall clock has ended, and publishes every interval it passed while the peer was silent as an empty segment with `record_count=0` and a `rib.<date>.<time>.gz` beside it, which is what design section 6 already required: a healthy but silent period is an empty segment that is written, not a hole. Until now the only things that closed an interval were the next record and the end of the session, so a peer that went quiet after its dump left `updates.<date>.<time>.gz.tmp` behind for as long as it stayed quiet and the intervals after it were named by no range at all. A view this session never observed is left alone, because `coverage` already calls that a gap and an empty segment would claim the router's table was empty. `./lab/quiet.sh` holds a session open and silent for three intervals and asserts two updates segments, two RIBs and a `verify` exit of 0; `collector::tests::a_silent_peer_publishes_each_interval_at_its_boundary` drives the same clock without wall time.
- **Recovery closes a range no earlier than the range opened.** `recover` closed the writer's open range one microsecond after the last frame the journal proves, which is now older than the range itself when the interval was opened at a boundary the writer observed with no records: the closing transition went backwards, the checksummed timeline discarded that line, and every later session refused to open the archive with `coverage log has 1 discarded line(s)`. It now uses `max(last frame, range start) + 1`. `recovery::tests::a_range_opened_after_the_last_frame_closes_forward` drives a collector past three boundaries and then runs recovery, asserting no line is discarded and the timeline stays contiguous; it fails on the previous expression.
- **`verify` no longer calls the interval being written a missing segment.** The last range in a coverage log belongs to the interval the writer is still filling, and its sidecar appears only when that interval closes, so a read-only `verify` against a serving archive reported `missing` on a healthy archive. It now prints `open <identity>: the interval this writer is still filling` as a note and keeps its exit code at 0.

## v0.1.0 - 2026-11-18

- **ADD-PATH sessions can be accepted per message.** `[archive] add_path` takes `reject` (the
  default, unchanged behaviour: the session ends at Peer Up naming the families and the interval
  keeps `mixed_add_path`) or `detect`. Under `detect` the session is accepted and every Route
  Monitoring message is classified on its own from the only evidence a PDU holds: whether the
  no-path-id or the 4-byte-path-id reading consumes its NLRI sections exactly. A message that
  fits both readings is skipped, counted, and names `add_path_ambiguous` on its interval; a
  message whose NLRI does carry path identifiers is written with subtype 8 or 9 and is kept out
  of the prefix-keyed RIB, which names `add_path_not_in_rib`. Both reasons are new members of the
  closed set in `docs/design.md` section 4 and are deliberately not gap reasons. Detection is a
  decision about the subtype, not about decoding: turning a path-identified NLRI into elements
  still waits on bgpkit-parser #363. `./lab/addpath.sh` covers both arms against the FRR ADD-PATH peer: the
  refusal, and then the archive under `detect`, where the segment must decode back to the four announced prefixes.

- **Loc-RIB views are routed from RFC 9069 type 3 frames.** `FrameKind::PeerUp` and
  `RouteMonitoring` carry `loc_rib` (the `F` flag) and, for updates, the family set the message
  names across classic NLRI, `MP_REACH_NLRI` and `MP_UNREACH_NLRI` (an end-of-RIB marker falls
  back to the family it closes). A frame whose type does not match the configured view ends the
  session with `PeerTypeForView` and marks the segment `peer_type_for_view`, so an adjacency
  frame can no longer be archived under a Loc-RIB name. `check` accepts the two views for a
  `bmp` client and refuses them with `rib = true` or on a `bgp` feed. Types 1 and 2 stay refused.
  A second, different Loc-RIB instance arriving while the first is still up is refused with
  `LocRibPeerAmbiguous`, because both arrive from the zero-filled peer address and the collector
  keeps one slot per view.

- **ADD-PATH is refused, not labelled.** `lab/addpath.sh` turns RFC 7911 on in the FRR lab. It
  found that the capability cannot decide the subtype: with `addpath-tx-all-paths` and one path
  per prefix, FRR sends plain NLRI, so a record written under subtype 9 claims a path identifier
  that is not there and decodes to zero prefixes. The collector now ends such a session with a
  message naming the families, and writes no record that claims an identifier it cannot prove.
  Two earlier versions of this feature were wrong in opposite directions (the sent OPEN, then
  the received OPEN); both are recorded here because the lesson is the same: a negotiated
  capability is a promise about a session, not a fact about a message.
- **RFC 8050 ADD-PATH.** A Peer Up now records which families the router said it can send
  several paths for (its sent OPEN, send or send-receive). A Route Monitoring message whose
  families are all ADD-PATH is written under subtype 8 or 9; one that mixes ADD-PATH and plain
  families has no single honest subtype, so it is not encoded and its segment carries
  `mixed_add_path`. A `feed = "bgp"` peer that negotiated ADD-PATH is refused at OPEN time,
  because this feed does not read path identifiers and the plain subtype would make every
  reader take one for a prefix length. `peer_state` rows now carry the negotiated answer with
  `add_path_source = "sent_open"` instead of `unknown`.

- **Repair runs when the process starts.** Recovery, peer-state rebuild, segment republish,
  retention and quarantine resume used to run only when a session opened, so a crash that was
  never followed by a reconnect left its claim unrepaired and `verify` reported it missing.
  The whole pass now also runs once at startup for every configured client, before any listener
  is bound. It is idempotent: a rebuilt claim is cleared, so the run at session start finds
  nothing to do.

- **`verify` reconciles the catalog with the sidecars.** A row whose sidecar is gone is
  `catalog_stale`; a sidecar whose segment the catalog does not name is `catalog_missing`.
  Section 8 says the catalog is a projection, and a projection that disagrees with its source in
  either direction is the finding a reader would otherwise discover by trusting it.

- **`max_prefixes` is enforced.** It was parsed, compared by reload, and never read. Section 4
  makes it "a hard ceiling per AFI/SAFI", which is the size of the table the peer currently
  claims, so the feed tracks announced prefixes per family and removes withdrawn ones; crossing
  the ceiling ends the session with a message naming the family and the count, rather than
  archiving a partial table as if it were the whole one.

Correctness, in the order the panel found them.

- A route re-announced from the same peer replaces the stored attributes instead of keeping the
  old AS path and next hop while the sidecar claims `complete`.
- `verify` reports a missing root, a RIB with no coverage claim, a quarantined segment that
  should count as withheld, and a segment another collector's coordinates were masking, instead
  of printing `clean` for each.
- `status` exits non-zero when it printed findings.
- The catalog is a keyed projection with `part_seq` and `row_version`, one part per table and
  day, parts created with `create_new`, a `catalog/GENERATION` manifest from a rebuild, and a
  rebuild that adds parts instead of deleting the catalog and refuses to skip a sidecar it
  cannot read.
- A session's journal is a sequence of parts, rotated on `journal_max_bytes` or
  `journal_rotate_interval_minutes`, with `replay_parts` proving the sequence across them. A
  coverage line flushes the journal first, so a claim never precedes its evidence.
- Journal frames are fsynced in a byte-bounded batch (`journal_sync_bytes`) instead of one
  fsync per frame.
- A partial segment says so in its name: `updates.<date>.<time>.partial[.<n>].gz`, and the same
  for RIBs.
- A segment a killed writer claimed but never published is rebuilt from its journal as partial
  with `writer_restart`, so a crash no longer means a permanent hole.
- `session.json` records the source address, start and end, end reason, counters and the
  router's Initiation strings, and is refreshed at every segment close.
- A RIB whose interval had a refused update is published `partial` with `mixed_add_path`.
- `run` refuses to reuse a session epoch, takes an exclusive claim on `archive/.lock`, forwards
  SIGHUP to the serving session, and reports a session that ended.
- `check` refuses a duplicate listen address and a root that is not a directory, and prints a
  neighbour snippet for a `feed = "bgp"` client instead of a BMP block it cannot use.
- `quarantine` refuses a path outside the archive and records the path relative to the root.
- `bgp_id` is a client key: required for a `feed = "bgp"` client and used as the identifier in
  our OPEN.
- The catalog's segment bounds are `interval_start`/`interval_end`, the names section 8
  publishes.
- A journal frame carries a kind byte, so a replay knows whether it is reading a BMP message or
  a BGP one, and a BGP journal is refused by a rebuild instead of being read as BMP. The magic
  is the `BGPVJNL1` section 6 names; a part written before this change cannot be read.
- A rebuild no longer inherits the live ingest stall guard, which could mark a segment
  `backpressure` when the disk was merely slow while the frames were all read correctly.
- The frame layout is one implementation shared by the session journal and `peer_state.log`, so a
  second offset table cannot drift from the first.
- `peer_state.log` is written: one versioned canonical JSON payload per peer object, fsynced on
  Peer Up, Peer Down, EoR and at segment close, version counters seeded from the log so a restart
  cannot reuse one, and a version only when something a reader can see has changed.
- A Peer Up naming a peer type other than Global (RD, Local, Loc-RIB) is refused before any MRT
  output, marks the open segment `unsupported_peer_type` and ends the session. Such an instance
  has no global Adj-RIB-In, and archiving it into one put routes in a table they never belonged to.
- `loc-rib` and `loc-rib-filtered` are refused by `check` rather than accepted and silently left
  empty: this build routes no RFC 9069 Local Route Monitoring frames.
- Output is written through one helper that treats a closed pipe as done rather than as a crash,
  so `bgpviews status | head` exits 0 instead of panicking with "failed printing to stdout".
- A claimed range that holds no frames is rebuilt as an empty segment whose path exists: the
  empty-segment path returned the writer's original file name instead of the published one, so
  the rebuild failed with NotFound for exactly the claim it was written for.
- A segment's sidecar is synced before its directory entry is, so a power loss cannot leave a
  published MRT whose description never reached the disk.
- A `feed = "bgp"` session sends a KEEPALIVE every 30 s instead of one after its OPEN, so a healthy
  peer no longer has its session closed at the advertised 90 s hold time.
- A segment repair is written down before the coverage log stops claiming the range, and read back
  by the next recovery, so a crash between ordering the repair and doing it no longer loses the
  work. The record is removed once the segment is published.
- A rebuild refuses an epoch whose journal has a hole: a missing part number, or a part whose
  first sequence does not follow the one before it, answers `Unresolved` instead of publishing a
  segment built from the parts that survived.
- A republish locates the epoch's journal before it removes a sidecar-less artifact; removing
  first destroyed the only copy of a segment on the path where the rebuild then reports that the
  journal is not retained.
- A republish reads a rotated epoch (several journal parts) in order and rebuilds the same
  segment, proven by a test that rotates a live collector's journal and kills it.
- A republish streams the epoch's journal one part at a time instead of holding every frame in
  memory, so the rebuild's memory ceiling no longer grows with how long the dead session ran.
- A peer-state version lost between its write and its fsync is rebuilt from the journal when a
  session is repaired, and every rebuilt version carries `reconstructed: true` so a reader can
  tell an observation from a reconstruction.
- `verify` reports a cut peer-state log: the surviving version count and the `.torn` file that
  holds the bytes after the cut. A record nobody looks for is not a record.
- A torn `peer_state.log` is cut back to its last whole frame when the session is reopened, and
  the dropped bytes are kept as `peer_state.torn`. Appending behind a torn frame used to make
  every version after the tear unreadable.
- The catalog's fifth table, `peer_state`, is projected from the session logs: one row per version
  with the state, the EoR and Peer Down times, and the journal frame each came from.
- `bgpkit-snap` is pinned above the yanked release candidates and adapted to 0.2.

Unverified:

- Loc-RIB views. A Loc-RIB peer's frames are not routed yet.
- Exporter templates for Junos, IOS-XR, SR OS and EOS are shipped but untested; only FRR has
  produced the captures this repository's tests replay.
