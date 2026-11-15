# bgpviews lab experiments

Status: measured 2026-11-15 on the host that runs the lab, FRR 8.4_git in Docker,
with the code at the commit that added this file. Every number below came from a
command in this document, and every fixture it cites is committed under `lab/fixtures/`.

## What "proven" means here

An experiment counts only if three things exist: a scripted run anyone can repeat,
a committed artifact the test suite replays, and an independent reader that agrees.
Where a claim cannot meet that bar it is listed under **Not proven** with the reason,
not softened.

The independent readers are `bgpdump` and `monocle`. Neither shares code with the
writer, so a round trip between our own writer and our own reader would not have
been evidence.

## The lab

Two FRR routers on one Docker bridge, `r1` AS65001 and `r2` AS65002, peered over
IPv4 with an IPv6 family also negotiated. `r2` exports BMP pre-policy to a listener
on the host, which writes the byte stream verbatim.

```text
./lab/run.sh                 # up, capture, verify, down. 21 s from a clean state.
./lab/scenario.sh churn      # same, plus a withdrawal mid-capture
./lab/scenario.sh restart    # same, plus a session bounce mid-capture
```

Prerequisites and teardown are in `lab/README.md`. The image is pinned by digest in
the fixtures' provenance: `frrouting/frr:latest` resolved to FRR 8.4_git,
`sha256:990e8349...`.

## E1. A BMP session produces a faithful, parseable archive

Question: does a real router's BMP stream become a directory that existing tools
read without a service in the middle?

Measured: a 15 s capture is **1076 bytes, 10 BMP messages**: 1 Initiation, 1 Peer
Up, 8 Route Monitoring, of which 4 carry the four test prefixes and 4 are EoR
markers. Replayed, it becomes one segment of **8 MRT records (201 bytes gzipped)**,
a sidecar whose `sha256` matches `sha256sum`, a 10-frame journal, and a 3-range
coverage log.

Independent readers: `bgpdump -m` and `monocle parse` each report **4
announcements**, with AS path `65002 65001`, and the sidecar's hash equals the
hash of the published file.

Conclusion: the artifact chain works end to end, and its two readers disagree with
neither the writer nor each other.

## E2. Withdrawals survive the path

Question: does a withdrawal reach the archive as a withdrawal, or as something the
reader has to infer?

Method: `./lab/scenario.sh churn` withdraws `198.51.100.0/24` and `203.0.113.0/24`
on `r1` five seconds into the capture.

Measured: **1226 bytes, 12 BMP messages**; the two withdrawals appear as Route
Monitoring messages carrying IPv4 withdrawn NLRI. Replayed as **10 records**, which
`monocle` reads as **4 announcements and 2 withdrawals**.

Conclusion: a withdrawal is a record, not an inference, and the count is visible to
two independent readers.

## E3. A session restart splits a segment and keeps coverage contiguous

Question: when the monitored session drops and comes back inside one interval, does
the archive say so without either losing time or double-covering it?

Method: `./lab/scenario.sh restart` clears the BGP session on `r2` five seconds in.

Measured: **2032 bytes, 18 BMP messages**: 1 Initiation, 2 Peer Up, 1 Peer Down, 14
Route Monitoring. Replayed, it produces **two segments in the same interval**:
ordinal 1 with 8 records and reasons `peer_restarted, peer_vanished`, ordinal 2 with
6 records and reason `session_ended`. The paths differ only by the ordinal suffix
(`...0015.gz`, `...0015.2.gz`).

The coverage log is **4 ranges**, contiguous, one open: gap `no_session`, segment
ordinal 1, segment ordinal 2, gap `session_ended`. `verify`-level invariant checking
passes on the replayed file.

Conclusion: the peer going away and coming back is representable without inventing a
third state, and the timeline stays a partition.

## E4. Interval rollover closes one segment and opens the next

Question: does the collector roll over on the configured boundary, or only when the
capture ends?

Method: replay with a 60 s interval and 10 s spacing, so the 10 frames span two
intervals. (One minute is the shortest interval the design allows, and the code now
refuses anything shorter; see E5.)

Measured: **2 segments**, `...0023.gz` with 6 records and `...0024.gz` with 2. The
first closed at its configured boundary and is `complete` with no reasons; the second
closed by the end of the capture and is `partial` with `session_ended`.

Conclusion: `complete` is reachable, and it is reachable for the reason the design
says it should be: a boundary close with nothing wrong.

## E5. An interval shorter than a minute is refused

Question: what happens with a sub-minute interval, which the design forbids because
segment names have minute resolution?

Measured before the check existed: with a 1 s interval, eight records produced eight
segments that all resolved to the same file name and silently overwrote each other.
Measured now: `InvalidInterval { seconds: 1 }` before any file is written.

Conclusion: a configuration the design forbids produced data loss when it was
accepted, and refusing it is cheaper than disambiguating names.

## E6. Replay throughput

Question: is one process enough to archive what one router exports?

Method: `cargo run --release --example replay_bench -- N` replays one real Route
Monitoring frame N times behind a real Initiation and Peer Up, so the measurement is
about the writer (framing, journal, BGP4MP envelope, gzip, coverage) rather than about
a shape no router sends.

Measured on this host:

```text
records     20,000    200,000
wall        0.120 s   0.387 s
records/s   166,169   516,915
input MB/s  17.8      55.3
peak RSS    -         77 MB
```

The first measurement of the same code path took **more than 180 s for 100,000
records** because the journal fsynced once per frame. The design asks for a bounded
batch with one fsync; the writer now buffers and exposes `sync()` for the batch
boundary. That is a 400× difference on the same machine, and it is the one place
where a correctness-looking choice was a performance defect.

Conclusion: at 500k records/s the replay path is not the bottleneck for a single
router, and the fsync boundary is where the cost lives.

## E7. The MRT length field, measured rather than read

Question: does the MRT record's length field include the 12-byte common header?

RFC 6396 section 2 reads as if it does. Measured: a record written that way makes
**both `bgpdump` and `monocle` read exactly one record** and then stop with
"truncated MRT body: expected 65002 bytes, read 528", because the second record's
bytes are read as a length. Written the other way (`length` = everything after the
common header, including the 4-byte ET microsecond field), both readers report all
records.

Corroboration: `bgpkit-parser` states the convention in `parser/mrt/mrt_header.rs`,
and `focl` writes `length = payload.len()`.

Conclusion: the ecosystem convention is not the RFC's wording, and a writer that
follows the wording produces a file that looks valid and reads as one record.

## E8. The live socket path

Question: does the collector work when the bytes arrive over TCP from a real router
rather than from a file?

Method: `./lab/live.sh 20` builds the release binary, starts `bgpviews run` with
`lab/collector.toml`, brings the lab up, lets the session run, tears the lab down
(which closes the TCP connection), and waits for the collector to exit on its own.

Measured: the collector accepted the connection from `172.31.250.12`, archived **9
BMP frames in this order: Initiation, Peer Down, Peer Down, Peer Up, 4 Route
Monitoring, Peer Down**, and published one segment of **4 MRT records (156 bytes
gzipped)** with reasons `peer_vanished, session_ended`. `monocle` and `bgpdump` each
read 4 announcements. The coverage log is the usual 3 ranges: gap `no_session`,
segment ordinal 1, gap `session_ended`.

The two Peer Downs before the Peer Up are what a BMP target does when it connects
to a collector that has no state for it: it reports the peer it is monitoring as
down before it establishes the monitoring relationship. The collector records them,
finds no open segment to mark, and continues. That sequencing is visible in the
journal and did not need special handling, which is the useful result: the state
machine survives a connection that starts mid-history.

**Answered, E9.** This run produced no end-of-rib marker, and neither did the
collector in any run where it attached before the lab started. The question was
whether that was our collector's fault; it is not.

## E9. EoR is a property of the dump, not of the peer

Question: why did one listener see EoR and the other not, on the same lab?

Method: four runs in each of two orders, counting EoR markers by decoding the
journal (bgpviews) and the raw stream (the Python listener) with two independent
readers, two repetitions per order:

- **attached first**: the listener or collector is accepting before the lab starts.
- **attached after**: the lab is up and the BGP session is Established first.

Measured, `lab/eor_order.sh` (bgpviews) and `lab/eor_control.sh` (Python listener):

```text
bgpviews            frames  records  eor
  attached first       9       4      0
  attached first       9       4      0
  attached after      11       8      4
  attached after      11       8      4

python listener     frames  records  eor
  attached first       8       4      0
  attached first       8       4      0
  attached after      10       8      4
  attached after      10       8      4
```

Both implementations show the same split, so the behaviour belongs to FRR.

Conclusion: **FRR forwards an end-of-RIB only when it dumps its own RIB to a newly
attached BMP target.** It does not forward the monitored peer's EoR, so a collector
that is already listening when the peer comes up never sees one. The four EoR in the
"attached after" runs are two per family from that dump; whether they carry the
post-policy flag varies by run, which is why the earlier capture showed two pre and
two post.

This has a consequence the design has to face. bgpviews is a daemon: in the field it
will be listening when a router's sessions come up, which is exactly the case that
produces no EoR. `complete` currently requires an EoR per family, so on a live
collector `complete` is reachable only after the collector itself restarts and
re-attaches to an established session. That is backwards from what an operator wants:
the archive would be `complete` after an outage and `partial` while everything is
healthy.

Three ways to handle it, none of which I picked unilaterally:

1. Keep `complete` as specified and document the constraint. Cheapest, and honest,
   but it means the normal state of a healthy archive is `partial`. Operators will
   learn to ignore the word.
2. Add a named reason, `eor_not_forwarded`, for the case where the peer reached
   `synced` and no EoR can ever arrive because the target attached first. The segment
   stays `partial`; the reason says why, and `status` can separate "still dumping"
   from "finished, unprovable".
3. Replace the EoR requirement with a heuristic (quiet period after the initial
   burst). The design forbids guesses, so this one needs the design changed first,
   not just the code.

My recommendation is 2 now and 1 as the documented caveat, because it costs one
reason string and turns a permanent `partial` into an explained one.

Recorded separately: a pre-policy collector receives FRR's two post-policy EoR
duplicates and counts them as `unconfigured_view` instead of archiving them, which
is why the replay of the 10-frame capture writes 6 records, not 8.

## E10. Is the missing EoR a bug, and which FRR does what

Question: is "no EoR when the collector attaches first" an FRR defect, and is the
duplicate post-policy EoR one too?

Method: the same two orders, IPv4 unicast only, three implementations:
`frrouting/frr:latest` on Docker Hub (FRR **8.4_git**, digest
`sha256:990e8349...`), and `quay.io/frrouting/frr:10.7.1` (FRR 10.7.1_git), both
counted by the Python listener through `lab/eor_version.sh`. The IPv4-only config
exists because FRR 10.x refuses an IPv6 address family in a container with no local
IPv6 address, so the two versions can only be compared on one family.

```text
listener attached first      0 EoR      (8.4)     0 EoR      (10.7.1)
listener attached after      2 EoR      (8.4)     1 EoR      (10.7.1)
                             flags 0x00,0x40        flag 0x00
```

**The duplicate is a bug, and it is already fixed upstream.** 8.4 emits one EoR with
the pre-policy flag and one with `L` set (post-policy) even when only pre-policy is
monitored. FRR commit `d327af6` (merge of PR #19071, 2025-07-01, "bgp_bmp: do not
send end of rib by default") guards each EoR on the target's monitored families, and
10.7.1 sends exactly one. Our pinned image predates that fix by roughly a year, which
is why the earlier capture showed a post-policy EoR for a view that never had a Peer
Up.

**The missing EoR is not a bug, in either version.** Both FRR versions send an EoR
only from the initial dump they perform when a BMP target attaches, and FRR's source
shows `bmp_eor` has no other caller: there is no path that forwards the monitored
peer's own EoR. That matches RFC 7854, which ties the marker to the dump:

- section 3.3: the router sends the Adj-RIBs-In "once it has sent all the routes for
  a given peer, it MUST send an End-of-RIB message for that peer".
- section 5: "When the initial dump is completed for a given peer, this MUST be
  indicated by sending an End-of-RIB marker for that peer."

A peer that becomes Established after the BMP session opened is never dumped: it has
no Peer Up at session start, so there is nothing to dump and no EoR to send. The RFC
does not require forwarding the peer's BGP EoR either, so a collector that is already
listening can never learn that the table finished.

**Verdict.** Duplicate EoR: implementation bug, fixed after 8.4, so our handling of it
is now a compatibility rule for old exporters rather than a protocol rule. Missing
EoR when attached first: normal behaviour under RFC 7854, and unchanged in the
current release, so it is a protocol gap that archives must live with, not something
to wait out.

That settles the design question from E9 on the facts: `complete` cannot depend on an
EoR the router is never required to send. What to do about it is still a decision, not
a measurement, and it is listed with its options in E9.

## E11. You cannot know the dump finished, unless you make it happen

Question: without an EoR, how would any collector know the initial table is complete?

It cannot. Nothing in the BMP stream marks the end of a dump that the router never
performed, and silence afterwards is not evidence: an idle peer, a converging table
and a filtered one look the same. So the honest statement is that completion is
unobservable in that case, not that it happened and could not be proved.

The collector does have one lever: it controls when the target connects, and a target
that connects to an Established peer gets a dump with an EoR. Measured with
`lab/reconnect_dump.sh`, which attaches a listener, closes it, then attaches another
on the same port while the BGP session stays up:

```text
listener A, first attach    frames=10  records=8  eor=4
listener B, reconnect       frames=10  records=8  eor=4
```

A reconnect reproduces the full dump and its EoR. So a daemon can obtain a completion
signal by closing its own connection and letting the router redial, at the cost of one
extra full-table transfer per event, a gap in coverage between the two connections,
and the need for a cooldown so a flapping peer cannot trigger dumps repeatedly.

That changes the decision from "how to describe an unprovable state" to "should the
collector force the dump". `docs/design.md` records the question with both answers;
the choice is an operator-facing trade-off, not a measurement.

## What the experiments changed in the design

Three rules moved because a measurement contradicted the text, not because a reviewer
preferred different wording:

1. **EoR in an unobserved view is not an error.** FRR emits each family's EoR twice,
   once with the post-policy flag set, even with only pre-policy monitoring
   configured. The old rule would have raised `peer_state_error` on every session
   against this implementation. Now it is counted as `eor_unobserved_view` and ignored.
2. **The length field convention** (E7) is now stated in section 19 as a compatibility
   fact, not left to the RFC.
3. **The journal fsyncs per batch, not per frame** (E6), which is what section 6
   always said and what the code did not do.
4. **EoR is a dump-completion marker the router need not ever send** (E9, E10). The
   design's completeness rule depends on it, so a daemon that listens before a peer
   establishes cannot reach `complete` by waiting; it has to name the state.
5. **The duplicate post-policy EoR is an FRR <=9.x bug, not vendor design** (E10), so
   the rule that tolerates it is documented as an old-exporter compatibility rule.

Recorded in section 19 as measured, not as design intent: FRR re-encodes UPDATEs for
BMP and prepends its own AS (a route learned as `65001` appears as `65002 65001`), so
an FRR pre-policy archive is faithful to what the router exported and not to what it
received. It also needs `soft-reconfiguration inbound` for pre-policy BMP, loads BMP
as a module (`-M bgpd_bmp`), and fails to fork under an inherited large
`RLIMIT_NOFILE`.

## Not proven

- **Any vendor other than FRR 8.4.** Junos, IOS-XR, SR OS and EOS are untested. FRR
  9.1 is pinned out: in a Docker container with no local IPv6 address it rejects the
  IPv6 family and the session stays Active/Idle.
- **A daemon.** `run` serves exactly one session per configured client and exits when
  that session ends. Reconnection, epoch allocation across restarts, SIGHUP, signals,
  backpressure, disk guards, clock steps and gap closure while a view is idle are
  designed and unexercised.
- **Whether any router forwards the monitored peer's EoR.** E9 shows FRR does not, and
  that EoR only accompanies the router's own dump. Only FRR was tested; whether this
  is one implementation's behaviour or the norm across vendors is unknown, and it
  decides whether `complete` needs a design change or a vendor note.
- **Whether an FRR knob changes it.** No configuration was found that makes FRR
  forward the peer's EoR; the search was not exhaustive.
- **RIB derivation.** `bgpkit-snap` is used for a synthetic prefix index
  (1.2M entries, 11 MB, 102 ms write, 8 ms open), not for a RIB built by the
  collector, because there is no collector trie yet.
- **Crash during publication.** Truncation and checksum failures are covered at the
  unit level, and a torn journal tail is covered by test. Killing a writer between
  the sidecar rename and the MRT rename has not been done against a real archive.
- **More than one peer.** The lab has one monitored peer. Multi-peer generation
  numbering, per-peer tries, and the table-dump peer index are untested.
- **Long runs and retention.** No capture longer than 22 s, so nothing here says
  anything about a week of churn or about expiry.

## Reproducing

```sh
cd lab
./run.sh                      # E1
./live.sh 20                  # E8, the real collector against the lab
./scenario.sh churn           # E2
./scenario.sh restart         # E3
cd ..
cargo test --test replay_fixture --test lab_scenarios
cargo run --release --example replay_fixture -- /tmp/out --interval 60 --spacing 10000000  # E4
cargo run --release --example replay_bench -- 200000                                        # E6
```
