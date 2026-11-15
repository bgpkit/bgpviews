//! Collector internals for bgpviews.
//!
//! Modules here are the parts of the design that must be identical in every
//! process that touches an archive: the coverage log's line format and the
//! checksum it is framed with. Nothing in this crate talks to a socket, opens a
//! configuration file, or decides policy.

pub mod coverage;
pub mod crc;

pub use coverage::{Line, LogError, Range, RangeKind, ReplaySummary, SegmentRef, Stamp, Timeline};
pub use crc::{crc32c, Crc32c};
