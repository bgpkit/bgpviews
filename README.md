# bgpviews

Self-hosted BGP archive. A router exports BMP to one binary; the binary writes
RouteViews-shaped MRT into a plain directory and leaves a Parquet catalog beside
it, so monocle, bgpkit-parser, bgpdump and DuckDB read the result without a
service in the middle.

Site: <https://bgpviews.org> · Design: [`docs/design.md`](docs/design.md) ·
Features and their proofs: [`docs/features.md`](docs/features.md) · Changes:
[`CHANGELOG.md`](CHANGELOG.md) · Licence: MIT

## Status

Working. The newest tagged release is v0.1.6, dated 2026-11-25, and the v0.x line may still change
its on-disk format. Every feature in [`docs/features.md`](docs/features.md) has a proof listed
beside it, and the rows that are not done say so. `docs/design.md` is the contract this code is
written against.

Known open rows, all tracked in [`docs/features.md`](docs/features.md):

- RIB snapshots are off by default and, when on, are written on their own cadence
  (`rib_interval_minutes`, default 480) rather than at every update boundary. The update interval
  defaults to 5 minutes, the cadence RIPE RIS publishes at.
- A BMP peer that negotiated ADD-PATH is refused by default, with the families named. The
  capability cannot decide the BGP4MP subtype: FRR sends plain NLRI when it has one path per
  prefix, so a record labelled "path identifier" would be unreadable and nothing in the file
  would say so. `[archive] add_path = "detect"` accepts such a session instead and decides the
  subtype per message from the bytes; see `docs/design.md` sections 4, 8 and 18.
- `loc-rib` and `loc-rib-filtered` are routed from RFC 9069 type 3 frames, one instance per
  client; a second instance is refused rather than folded in. Several instances on one client
  are not told apart yet. The pinned FRR image is 8.4_git and has no `bmp monitor loc-rib`
  keyword, so the path is covered by parse tests and `config`, not by a capture.
- A journal written before v0.1.0 is not readable: the magic and the frame layout changed before
  the first tag, so an archive has to be built with one build. Every tagged v0.1.x release reads
  the journal and sidecar layout of the one before it, and a v0.1.5 archive opens under v0.1.6
  without migration.
- A live `run` appends three of the five tables section 8 names (`segments`, `peers` and closed
  `gaps`); `sessions` and `peer_state` are written by `rebuild-index`, because a live append of
  those two would rewrite rows a reader may be holding. `peer_state.log` exists for `feed = "bmp"`
  sessions only.
- A peer-state version lost between its write and its fsync is rebuilt from the journal when the
  next session repairs the archive, and marked `reconstructed`. Versions lost from an epoch whose
  journal was already expired are gone; the `.torn` file is all that remains of them.
- Only FRR has a snippet and a capture. Junos, IOS-XR, SR OS and EOS have neither, so `check`
  prints nothing for them rather than an untested template.

## Installing

Three ways, none of which needs a database, a service or an account:

```sh
# Container: a named volume holds the archive, so the unprivileged user inside the
# image owns it without any chown on the host
docker run --rm -p 17901:17901 \
  -v bgpviews-archive:/var/lib/bgpviews \
  -v "$PWD/bgpviews.toml:/etc/bgpviews/bgpviews.toml" \
  bgpkit/bgpviews

# Prebuilt binary from a tagged release: linux x86_64 and linux aarch64, both statically
# linked against musl, plus a universal macOS binary
# https://github.com/bgpkit/bgpviews/releases

# From source
cargo install --git https://github.com/bgpkit/bgpviews bgpviews
```

The container runs as uid 999 (`bgpviews`), never as root. A host bind mount works instead of the
named volume, but then the directory has to be writable by that user: `chown -R 999:999 ./archive`
first, or add `--user "$(id -u):$(id -g)"` to run as yourself. Without that the first write fails
with `cannot open .../.lock: Permission denied`.

`bgpkit/bgpviews:main` is built from every push to main; a tag publishes `{version}`,
`{major}.{minor}` and `latest`. Release binaries are attached to each GitHub release from v0.1.2 onward; v0.1.0 predates that
workflow and v0.1.1's tag build failed on a newer clippy lint, so either of those carries none. The
Linux binaries are static from v0.1.3: they run on any distribution, whatever its glibc.

The binary needs nothing else: no database, no service, no account. `bgpviews check` prints the
router snippet and the firewall rule for the host it runs on, and `bgpviews run` writes into the
archive root from the configuration file.

## The router side

`bgpviews check` prints the snippet for the client it was given, and what it prints depends on
the feed, not on the vendor:

| What `check` prints | When | Tested against |
|---|---|---|
| an FRR `bmp targets` block for the configured view: `adj-in-pre`, `adj-in-post`, `loc-rib`, `loc-rib-filtered`, or `adj-out` from the RFC 8671 `O` flag | a `feed = "bmp"` client | FRR 8.4_git in the lab, every capture in `lab/fixtures/` |
| a `neighbor ... remote-as` block for the collector's AS | a `feed = "bgp"` client | the lab's BGP feed, in `lab/live.sh` |

Design section 17 asks for Junos, IOS-XR, SR OS and EOS templates as well. They are not built:
`check` prints no snippet for them, because a template that has never produced a capture is a
guess printed in a configuration language. When one of them is added it will carry its capture,
the way the FRR block does.

## Reading the archive without this tool

An archive is readable by anything that reads MRT: `monocle parse`, `bgpkit-parser`, `bgpdump`,
or a DuckDB query over the catalog. Point a reader at `updates.<date>.<time>[.partial].gz` for
BGP4MP updates, at `rib.<date>.<time>[.partial].gz` for a table dump, and at
`catalog/<table>/**/*.parquet` for the index. A segment whose name carries `.partial` is a
segment that was cut short, and its sidecar says why.

## Commands

```text
bgpviews run --config FILE            archive sessions; --once serves one session per client
bgpviews check --config FILE          validate the configuration, print router snippets
bgpviews status --config FILE         coverage, segments, reasons and baseline, from the archive
bgpviews verify --config FILE         reconcile the archive against itself, both directions
bgpviews rebuild-index --config FILE  rebuild the catalog from the published sidecars
bgpviews quarantine --config FILE --path FILE   withhold an artifact and record why (--reason)
```

Exit codes: 0 clean, 1 findings, 2 configuration error, 3 I/O error or another writer.

`verify` is read-only and safe against a serving archive. The interval the writer is still filling
has no sidecar yet, so it is reported as `open <identity>: the interval this writer is still
filling` and not as a finding.

## Quick start

```toml
[archive]
root = "/var/lib/bgpviews"
# How a peer that announced ADD-PATH is handled: "reject" (default) refuses the session and
# names the families; "detect" accepts it and chooses the BGP4MP subtype per message.
add_path = "reject"

[[clients]]
name = "r2"
listen = "0.0.0.0:17901"
allow_from = ["192.0.2.1/32"]
feed = "bmp"

[clients.view]
name = "adj-in-pre"
updates_interval_minutes = 5   # default: the cadence RIPE RIS publishes updates at
rib = false                    # default: no snapshots; turn it on if you want tables
# rib_interval_minutes = 480   # RIS's 8-hour RIB cadence, only read when rib = true
```

Every boundary is a whole multiple of the interval on the UTC clock, so a 5-minute archive writes
`updates.<date>.1200.gz` and `updates.<date>.1205.gz`, and an 8-hour `rib_interval_minutes` lands at
00:00, 08:00 and 16:00 UTC. `rib_interval_minutes` must be a whole multiple of
`updates_interval_minutes`; `check` refuses a pair that can never come due. RIB snapshots are off by
default because one full-table RIB is about 85 MB, which at the update cadence is gigabytes a day.

```sh
bgpviews check --config bgpviews.toml   # prints the FRR snippet and the firewall rule
bgpviews run --config bgpviews.toml
```

One archive root has one writer. `run` and `rebuild-index` take an exclusive claim on
`archive/.lock`, and a second writer exits 3 with `writer_conflict` naming the holder. A
claim left by a process that died is reported on the next start and reused, because the
kernel says the lock is free.

## What the archive looks like

```text
r2-adj-in-pre/bgpdata/2026.11/UPDATES/updates.20261115.1800.gz        MRT + .json sidecar
r2-adj-in-pre/bgpdata/2026.11/RIBS/rib.20261115.1830.gz               RIB + .json sidecar
coverage/r2-adj-in-pre/updates.jsonl                                 the time partition
sessions/r2/0000000000000001/journal.0.bin                           raw frames, per-frame CRC
sessions/r2/0000000000000001/session.json                            closed session record
catalog/segments/2026.11/20261115.000001.parquet                     the projection
catalog/GENERATION                                                   parts of the last rebuild
quarantine/r2-adj-in-pre/                                            withheld artifacts
tombstones/r2-adj-in-pre/                                            what retention removed
.lock                                                                 the single writer's claim
```

Coverage is a partition of time, not a status per interval: every instant of a collector's
life falls in exactly one range, and a range is either a `segment` that names an MRT file or
a `gap` that names why no file covers that time. An empty segment means "watching, nothing
happened"; a gap means "nothing was recorded, and here is the reason".

## Reading the catalog

Every catalog row carries `part_seq` and `row_version`. Parts are immutable: a rebuild adds
parts and never rewrites one, so current state is the row with the greatest `part_seq` per
key, and that is the published query for each table.

Segments, keyed by collector, stream, interval start and ordinal:

```sql
select * exclude (part_seq, row_version)
from read_parquet('archive/catalog/segments/**/*.parquet')
qualify row_number() over (
  partition by collector, stream, interval_start, ordinal
  order by part_seq desc, row_version desc
) = 1;
```

Sessions, keyed by client and epoch:

```sql
select * exclude (part_seq, row_version)
from read_parquet('archive/catalog/sessions/**/*.parquet')
qualify row_number() over (
  partition by client, epoch order by part_seq desc, row_version desc
) = 1;
```

Gaps, keyed by collector and gap start:

```sql
select * exclude (part_seq, row_version)
from read_parquet('archive/catalog/gaps/**/*.parquet')
qualify row_number() over (
  partition by collector, gap_start order by part_seq desc, row_version desc
) = 1
order by gap_start;
```

Peer baselines, keyed by collector, peer, family and generation:

```sql
select * exclude (part_seq, row_version)
from read_parquet('archive/catalog/peers/**/*.parquet')
qualify row_number() over (
  partition by collector, peer_ip, afi, safi, generation
  order by part_seq desc, row_version desc
) = 1;
```

The fifth table, `peer_state`, is written from those same logs: one row per version, keyed by
client, session epoch, peer key, generation, view and AFI/SAFI, with `state_version` in the key so
a reader takes the greatest version rather than the greatest part. `peers` is the other thing: the
per-generation baseline the segment sidecars carry.

## Correctness and the lab

Two things are part of the proof rather than an afterthought, and both live in this
repository:

1. **Crash behaviour.** The session journal, the coverage log and the segment transaction are
   specified so that every crash point has one defined result. Tests kill the writer at each
   step and assert the recovered state.
2. **A repeatable lab.** `lab/` stands up a real FRR router that exports BMP, captures it, and
   turns the capture into a fixture the test suite replays. A claim about vendor behaviour
   that the lab does not reproduce is marked unverified in the design, not assumed.

Every lab reports by exit code: `0` passed, anything else did not. `77` means the lab could not
run in this environment and skipped itself with a printed reason; **a skip is not a pass**, and
`docs/features.md` says which row a skipped lab leaves unverified.

`crates/bgpviews/examples/peer_state_versions.rs` prints a `peer_state.log` through the same
replay the catalog projection uses. Use it rather than a hand-written decoder: the framing means a
decoder that is one byte off still produces plausible JSON, and an invented third version looks
exactly like a real one.

After a run, restore the lab's own directories with `./lab/clean-run.sh`, never `git checkout --
lab/`: that also reverts uncommitted edits to `lab/*.sh`, and a commit then records a lab that
still asserts the opposite of what the code now does.

Run one lab at a time. Each lab rebuilds `target/release/bgpviews` and the BMP labs bind the same
fixed ports, so two labs started together race on the binary and on the listener. The symptom is
misleading: a lab can execute a binary from another run's build and report the other run's journal
format as corruption in this one, which is not a finding about the code. The rule is enforced
rather than remembered: every lab takes `flock` on `lab/.lab.lock` and waits for it
(`LAB_LOCK_WAIT` seconds, default 1800), so a second run queues instead of interleaving.

The lab pins `frrouting/frr:latest` from Docker Hub because that is the image every capture here
was taken against. The other images were measured here and do not work: with
`quay.io/frrouting/frr:9.1.0` the router's BMP target connects and the collector accepts the
session as epoch 1, but the r1 to r2 BGP session never reaches Established, so no monitored
peer exists and the BMP stream carries no records; `10.7.1` fails the same way. If the Docker
Hub image also stops working, the labs that need a live router skip with a printed reason
rather than fail, and the fix is a lab, not a collector change.

## Layout

```text
crates/bgpviews-core   library: journal, coverage log, catalog, encoders
crates/bgpviews        binary: run, check, status, verify, rebuild-index, quarantine
docs/design.md         the contract
docs/features.md       every feature with its proof and its open rows
lab/                   FRR + BMP capture, produces test fixtures
```

## Reuse

This is not a second implementation of things BGPKIT already has. The BGP4MP envelope and
the RouteViews table-dump work come from `focl`; BMP and MRT parsing come from
`bgpkit-parser`; file and compression transport comes from `oneio`. What is new here is the
collector: a BMP session owner, a coverage log, and a segment transaction that can be
replayed.
