# Changelog

Notable changes, newest first. This file starts with the work that has not been tagged yet:
nothing has been released, so every entry below is unreleased and the archive format may still
change.

### Unreleased

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
