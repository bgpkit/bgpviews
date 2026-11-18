# bgpviews

Self-hosted BGP archive. A router exports BMP to one binary; the binary writes
RouteViews-shaped MRT into a plain directory and leaves a Parquet catalog beside
it, so monocle, bgpkit-parser, bgpdump and DuckDB read the result without a
service in the middle.

Site: <https://bgpviews.org> · Design: [`docs/design.md`](docs/design.md) ·
Features and their proofs: [`docs/features.md`](docs/features.md) · Changes:
[`CHANGELOG.md`](CHANGELOG.md) · Licence: MIT

## Status

Working, unreleased. Every feature in [`docs/features.md`](docs/features.md) has a proof
listed beside it, and the rows that are not done say so. `v0.1.0` is the next tag; the
design is frozen at v1.0-rc1 and `docs/design.md` is the contract this code is written
against.

Known open rows, all tracked in [`docs/features.md`](docs/features.md):

- `check` accepts `loc-rib` and `loc-rib-filtered`, but the collector routes only global
  Adj-RIB-In frames, so those two views do not archive anything yet. Whether to route
  RFC 9069 Local Route Monitoring frames or to refuse the views is an open decision.
- The journal's magic is `BGPVPJ01` and its frames carry no kind byte, where design section 6
  says `BGPVJNL1` and one kind byte per frame. Nothing has been released, so this will change
  before the first tag, and a journal written by this build will not be readable after it.
- The catalog writes four tables; section 8 names five, and `peer_state` is the one missing.

## Installing

There is no published release yet, so a build is from source:

```sh
cargo install --git https://github.com/bgpkit/bgpviews bgpviews
```

The binary needs nothing else: no database, no service, no account. `bgpviews check` prints the
router snippet and the firewall rule for the host it runs on, and `bgpviews run` writes into the
archive root from the configuration file.

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
bgpviews quarantine --path FILE       withhold an artifact and record why (--reason)
```

Exit codes: 0 clean, 1 findings, 2 configuration error, 3 I/O error or another writer.

## Quick start

```toml
[archive]
root = "/var/lib/bgpviews"

[[clients]]
name = "r2"
listen = "0.0.0.0:17901"
allow_from = ["192.0.2.1/32"]
feed = "bmp"

[clients.view]
name = "adj-in-pre"
updates_interval_minutes = 15
rib = true
```

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
r2-adj-in-pre/bgpdata/2026.11/RIBS/ribs.20261115.1830.gz              RIB + .json sidecar
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

The design's fifth table, `peer_state`, is not written yet; `peers` holds the per-generation
baseline the segment sidecars carry.

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

The lab pins `frrouting/frr:latest` from Docker Hub because that is the image every capture here
was taken against. FRR 10.7.1 and 9.1.0 could not bring the lab's eBGP session up in this
environment; if the Docker Hub image also stops working, the labs that need a live router will
skip rather than fail, and the fix is a lab, not a collector change.

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
