# bgpviews

Self-hosted BGP archive. A router exports BMP to one binary; the binary writes
RouteViews-shaped MRT into a plain directory and leaves a Parquet catalog beside
it, so monocle, bgpkit-parser, bgpdump and DuckDB read the result without a
service in the middle.

Site: <https://bgpviews.org> · Design: [`docs/design.md`](docs/design.md) · Licence: MIT

## Status

**Design v1.0-rc1, frozen 2026-11-14. No code yet.** The design document is the
contract this repository is written against: eighteen adjudicated review rounds,
the last of which closed on the rule set rather than on scope. Nothing here
should be read as working software until a release says so.

## What the design promises

- One binary, one TOML file, one output directory. No database server, no queue,
  no second daemon.
- Coverage is a partition of time, not a status per interval: every instant of a
  collector's life falls in exactly one range, and a range is either a `segment`
  that names an MRT file or a `gap` that names why no file covers that time. An
  empty segment means "watching, nothing happened"; a gap means "nothing was
  recorded, and here is the reason". A reader can tell those apart, which is the
  question an archive is actually asked.
- `complete` is deliberately narrow and never claims what BMP cannot prove.
- The catalog is a projection of the sidecars and can be rebuilt from them.

## Correctness and the lab

Two things are part of the proof rather than an afterthought, and both live in
this repository:

1. **Crash behaviour.** The session journal, the coverage log and the segment
   transaction are specified so that every crash point has one defined result.
   Tests kill the writer at each step and assert the recovered state.
2. **A repeatable lab.** `lab/` stands up a real FRR router that exports BMP,
   captures it, and turns the capture into a fixture the test suite replays. A
   claim about vendor behaviour that the lab does not reproduce is marked
   unverified in the design, not assumed.

## Layout

```text
crates/bgpviews-core   library: journal, coverage log, encoders
crates/bgpviews        binary: run, check, status, verify, rebuild-index
docs/design.md         the contract
lab/                   FRR + BMP capture, produces test fixtures
```

## Reuse

This is not a second implementation of things BGPKIT already has. The BGP4MP
envelope and the RouteViews table-dump work come from `focl`; BMP and MRT
parsing come from `bgpkit-parser`; file and compression transport comes from
`oneio`. What is new here is the collector: a BMP session owner, a coverage log,
and a segment transaction that can be replayed.
