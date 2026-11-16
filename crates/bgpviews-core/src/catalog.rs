//! The catalog: the archive's own index, written as Parquet.
//!
//! Design reference: `docs/design.md` section 8. Three rules shape this file.
//!
//! **The catalog is a projection of the sidecars, never a second truth.** Every row can
//! be rebuilt from files that are already published, which is why `rebuild-index` is a
//! command rather than a repair tool: deleting `catalog/` must cost time, not data.
//!
//! **Tables are append-only files, one month directory with a part per day.** A rewrite of
//! the whole catalog on every publication would make the cost of a segment proportional to
//! the archive's age. `catalog/GENERATION` names the parts of the last rebuild, which is what
//! makes a rebuild atomic; a reader that wants current state globs the table and takes the
//! greatest `part_seq` per key, which is the query section 8 publishes.
//!
//! **Nothing here reads a journal.** A catalog row describes what was published; the
//! journal is the record of what arrived, and mixing the two would let an unpublished
//! frame appear in an index that claims to describe published files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{BooleanArray, StringArray, UInt32Array, UInt64Array, UInt8Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use crate::coverage::{RangeKind, Timeline};
use crate::segment::Sidecar;

/// What a rebuild wrote.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Segment rows.
    pub segments: usize,
    /// Session rows, one per `(client, epoch)`.
    pub sessions: usize,
    /// Gap rows, from the coverage logs.
    pub gaps: usize,
    /// Peer-baseline rows.
    pub peers: usize,
    /// Files written.
    pub files: Vec<PathBuf>,
}

/// Why a rebuild could not finish.
#[derive(Debug)]
pub enum CatalogError {
    /// A sidecar or coverage log could not be read.
    Io {
        /// What was being done.
        op: &'static str,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A Parquet file could not be written.
    Parquet(String),
    /// A sidecar that is not the JSON this code writes.
    Json {
        /// Which file.
        path: String,
        /// What the parser said.
        detail: String,
    },
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { op, source } => write!(f, "{op}: {source}"),
            Self::Json { path, detail } => write!(f, "{path}: {detail}"),
            Self::Parquet(detail) => write!(f, "parquet: {detail}"),
        }
    }
}

impl std::error::Error for CatalogError {}

/// Append one published segment to the catalog, without touching anything else.
///
/// This is what keeps a live archive's index current: each publication writes its own
/// small Parquet part under `catalog/<table>/live/`, and `rebuild` later compacts them
/// into one file per day. The alternative, rewriting the day file on every publication,
/// would make the cost of a segment grow with the archive's age.
///
/// Gaps are not appended here. A gap lives in the coverage log and is derived by
/// `rebuild`; a live index that claimed to know them would have to read and re-derive
/// every log on every publication.
///
/// # Errors
/// Returns [`CatalogError`] when a part file cannot be written.
pub fn append_segment(root: &Path, sidecar: &Sidecar) -> Result<Vec<PathBuf>, CatalogError> {
    let segments = [SegmentRow {
        row_version: 1,
        collector: sidecar.collector.clone(),
        client: sidecar.client.clone(),
        view: sidecar.view.clone(),
        feed: sidecar.feed.clone(),
        stream: match sidecar.stream {
            crate::segment::Stream::Updates => "updates".to_string(),
            crate::segment::Stream::Ribs => "ribs".to_string(),
        },
        path: sidecar.path.clone(),
        range_start: sidecar.range_start.clone(),
        range_end: sidecar.range_end.clone(),
        session_epoch: sidecar.session_epoch,
        ordinal: sidecar.ordinal,
        bytes: sidecar.bytes,
        records: sidecar.records,
        state: sidecar.state.clone(),
        partial_reasons: sidecar.partial_reasons.join(","),
        sha256: sidecar.sha256.clone(),
        late_arrival: sidecar.late_arrival,
    }];
    let mut written = Vec::new();
    written.push(write_part(root, "segments", &sidecar.range_start, |seq| {
        segments_batch(&segments, seq)
    })?);

    if !sidecar.baseline.is_empty() {
        let peers: Vec<PeerRow> = sidecar
            .baseline
            .iter()
            .map(|entry| PeerRow {
                row_version: 1,
                collector: sidecar.collector.clone(),
                peer_ip: entry.peer_ip.clone(),
                peer_asn: entry.peer_asn,
                generation: entry.generation,
                afi: entry.afi,
                safi: entry.safi,
                baseline: entry.baseline.clone(),
                range_start: sidecar.range_start.clone(),
            })
            .collect();
        written.push(write_part(root, "peers", &sidecar.range_start, |seq| {
            peers_batch(&peers, seq)
        })?);
    }
    Ok(written)
}

/// Append one closed gap to the catalog.
///
/// A gap is written when it *ends*: until then the range is still open, and a row claiming
/// an end that has not happened would be a guess. This is the same rule the coverage log
/// follows, which is why the two agree without recomputing each other.
///
/// # Errors
/// Returns [`CatalogError`] when the part file cannot be written.
pub fn append_gap(
    root: &Path,
    collector: &str,
    start: &str,
    end: &str,
    reason: &str,
) -> Result<PathBuf, CatalogError> {
    let rows = [GapRow {
        row_version: 1,
        collector: collector.to_string(),
        start: start.to_string(),
        end: end.to_string(),
        reason: reason.to_string(),
    }];
    write_part(root, "gaps", start, |seq| gaps_batch(&rows, seq))
}

/// Rebuild every catalog table from the published files under `root`.
///
/// The rebuild adds parts and never removes one: a part a reader may be holding is not the
/// rebuild's to delete, and the generation manifest is what makes the replacement atomic. It
/// fails rather than skipping: a sidecar it cannot read is a source object it cannot project,
/// and a catalog that silently drops one row is worse than a rebuild that did not finish.
///
/// # Errors
/// Returns [`CatalogError`] when a source object cannot be read or a part cannot be written.
pub fn rebuild(root: &Path) -> Result<Summary, CatalogError> {
    let catalog = root.join("catalog");
    std::fs::create_dir_all(&catalog).map_err(|e| CatalogError::Io {
        op: "create the catalog directory",
        source: e,
    })?;

    let mut summary = Summary::default();
    let mut segments = Vec::new();
    let mut sessions: BTreeMap<(String, u64), SessionRow> = BTreeMap::new();
    let mut peers = Vec::new();

    for path in sidecars(root) {
        let text = std::fs::read_to_string(&path).map_err(|e| CatalogError::Io {
            op: "read a sidecar",
            source: e,
        })?;
        let sidecar = serde_json::from_str::<Sidecar>(&text).map_err(|e| CatalogError::Json {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;
        let session = sessions
            .entry((sidecar.client.clone(), sidecar.session_epoch))
            .or_insert_with(|| SessionRow {
                row_version: 1,
                client: sidecar.client.clone(),
                epoch: sidecar.session_epoch,
                first_range: sidecar.range_start.clone(),
                last_range: sidecar.range_end.clone(),
                segments: 0,
                records: 0,
                stream_has_ribs: false,
            });
        if sidecar.range_start < session.first_range {
            session.first_range.clone_from(&sidecar.range_start);
        }
        if sidecar.range_end > session.last_range {
            session.last_range.clone_from(&sidecar.range_end);
        }
        session.segments += 1;
        session.records += sidecar.records;
        if sidecar.stream == crate::segment::Stream::Ribs {
            session.stream_has_ribs = true;
        }
        for entry in &sidecar.baseline {
            peers.push((
                sidecar.range_start.clone(),
                PeerRow {
                    row_version: 1,
                    collector: sidecar.collector.clone(),
                    peer_ip: entry.peer_ip.clone(),
                    peer_asn: entry.peer_asn,
                    generation: entry.generation,
                    afi: entry.afi,
                    safi: entry.safi,
                    baseline: entry.baseline.clone(),
                    range_start: sidecar.range_start.clone(),
                },
            ));
        }
        segments.push((
            sidecar.range_start.clone(),
            SegmentRow {
                row_version: 1,
                collector: sidecar.collector.clone(),
                client: sidecar.client.clone(),
                view: sidecar.view.clone(),
                feed: sidecar.feed.clone(),
                stream: match sidecar.stream {
                    crate::segment::Stream::Updates => "updates".to_string(),
                    crate::segment::Stream::Ribs => "ribs".to_string(),
                },
                path: sidecar.path.clone(),
                range_start: sidecar.range_start.clone(),
                range_end: sidecar.range_end.clone(),
                session_epoch: sidecar.session_epoch,
                ordinal: sidecar.ordinal,
                bytes: sidecar.bytes,
                records: sidecar.records,
                state: sidecar.state.clone(),
                partial_reasons: sidecar.partial_reasons.join(","),
                sha256: sidecar.sha256.clone(),
                late_arrival: sidecar.late_arrival,
            },
        ));
    }

    // Gaps come from the coverage logs, which is the only place a missing period is
    // recorded: a sidecar exists only for what was written.
    let mut gaps = Vec::new();
    for log in coverage_logs(root) {
        let bytes = std::fs::read(&log).map_err(|e| CatalogError::Io {
            op: "read a coverage log",
            source: e,
        })?;
        let (timeline, _summary) = Timeline::replay(&bytes);
        let collector = log
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        for range in timeline.ranges() {
            // A range with no end is the open one; it is written once, below, with an
            // empty end so a reader can tell "still missing" from "was missing".
            if range.kind != RangeKind::Gap || range.end.is_none() {
                continue;
            }
            let start = range.start.render();
            gaps.push((
                start.clone(),
                GapRow {
                    row_version: 1,
                    collector: collector.clone(),
                    start,
                    end: range.end.map_or_else(String::new, |end| end.render()),
                    reason: range.reason.clone().unwrap_or_default(),
                },
            ));
        }
        // The open range is a gap that has not ended yet; it is reported with an empty
        // end so a reader can tell "still missing" from "was missing".
        if let Some(open) = timeline.open_range() {
            if open.kind == RangeKind::Gap {
                let start = open.start.render();
                gaps.push((
                    start.clone(),
                    GapRow {
                        row_version: 1,
                        collector,
                        start,
                        end: String::new(),
                        reason: open.reason.clone().unwrap_or_default(),
                    },
                ));
            }
        }
    }

    summary.segments = segments.len();
    summary.sessions = sessions.len();
    summary.gaps = gaps.len();
    summary.peers = peers.len();

    let session_rows = sessions
        .values()
        .map(|row| (row.first_range.clone(), row.clone()))
        .collect::<Vec<_>>();
    summary
        .files
        .extend(write_rows(root, "segments", segments, |rows, seq| {
            segments_batch(rows, seq)
        })?);
    summary
        .files
        .extend(write_rows(root, "sessions", session_rows, |rows, seq| {
            sessions_batch(rows, seq)
        })?);
    summary
        .files
        .extend(write_rows(root, "gaps", gaps, |rows, seq| {
            gaps_batch(rows, seq)
        })?);
    summary
        .files
        .extend(write_rows(root, "peers", peers, |rows, seq| {
            peers_batch(rows, seq)
        })?);
    write_generation(root, &summary.files)?;
    Ok(summary)
}

#[derive(Debug, Clone)]
struct SegmentRow {
    row_version: u64,
    collector: String,
    client: String,
    view: String,
    feed: String,
    stream: String,
    path: String,
    range_start: String,
    range_end: String,
    session_epoch: u64,
    ordinal: u32,
    bytes: u64,
    records: u64,
    state: String,
    partial_reasons: String,
    sha256: String,
    late_arrival: u64,
}

#[derive(Debug, Clone)]
struct SessionRow {
    row_version: u64,
    client: String,
    epoch: u64,
    first_range: String,
    last_range: String,
    segments: u64,
    records: u64,
    stream_has_ribs: bool,
}

#[derive(Debug, Clone)]
struct GapRow {
    row_version: u64,
    collector: String,
    start: String,
    end: String,
    reason: String,
}

#[derive(Debug, Clone)]
struct PeerRow {
    row_version: u64,
    collector: String,
    peer_ip: String,
    peer_asn: u32,
    generation: u64,
    afi: u16,
    safi: u16,
    baseline: String,
    range_start: String,
}

fn text(name: &str) -> Field {
    Field::new(name, DataType::Utf8, false)
}

fn segments_batch(rows: &[SegmentRow], part_seq: u64) -> Result<RecordBatch, CatalogError> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("part_seq", DataType::UInt64, false),
        Field::new("row_version", DataType::UInt64, false),
        text("collector"),
        text("client"),
        text("view"),
        text("feed"),
        text("stream"),
        text("path"),
        text("range_start"),
        text("range_end"),
        Field::new("session_epoch", DataType::UInt64, false),
        Field::new("ordinal", DataType::UInt32, false),
        Field::new("bytes", DataType::UInt64, false),
        Field::new("records", DataType::UInt64, false),
        text("state"),
        text("partial_reasons"),
        text("sha256"),
        Field::new("late_arrival", DataType::UInt64, false),
    ]));
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from(vec![part_seq; rows.len()])),
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.row_version).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.collector.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.client.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.view.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.feed.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.stream.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.path.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|r| r.range_start.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.range_end.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.session_epoch).collect::<Vec<_>>(),
            )),
            Arc::new(UInt32Array::from(
                rows.iter().map(|r| r.ordinal).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.bytes).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.records).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.state.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|r| r.partial_reasons.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.sha256.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.late_arrival).collect::<Vec<_>>(),
            )),
        ],
    )
    .map_err(|e| CatalogError::Parquet(e.to_string()))
}

fn sessions_batch(rows: &[SessionRow], part_seq: u64) -> Result<RecordBatch, CatalogError> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("part_seq", DataType::UInt64, false),
        Field::new("row_version", DataType::UInt64, false),
        text("client"),
        Field::new("epoch", DataType::UInt64, false),
        text("first_range"),
        text("last_range"),
        Field::new("segments", DataType::UInt64, false),
        Field::new("records", DataType::UInt64, false),
        Field::new("has_ribs", DataType::Boolean, false),
    ]));
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from(vec![part_seq; rows.len()])),
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.row_version).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.client.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.epoch).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|r| r.first_range.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|r| r.last_range.clone())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.segments).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.records).collect::<Vec<_>>(),
            )),
            Arc::new(BooleanArray::from(
                rows.iter().map(|r| r.stream_has_ribs).collect::<Vec<_>>(),
            )),
        ],
    )
    .map_err(|e| CatalogError::Parquet(e.to_string()))
}

fn gaps_batch(rows: &[GapRow], part_seq: u64) -> Result<RecordBatch, CatalogError> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("part_seq", DataType::UInt64, false),
        Field::new("row_version", DataType::UInt64, false),
        text("collector"),
        // `start` and `end` are reserved words in SQL; a column a reader has to quote is
        // a column they will get wrong.
        text("gap_start"),
        text("gap_end"),
        text("reason"),
    ]));
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from(vec![part_seq; rows.len()])),
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.row_version).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.collector.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.start.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.end.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.reason.clone()).collect::<Vec<_>>(),
            )),
        ],
    )
    .map_err(|e| CatalogError::Parquet(e.to_string()))
}

fn peers_batch(rows: &[PeerRow], part_seq: u64) -> Result<RecordBatch, CatalogError> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("part_seq", DataType::UInt64, false),
        Field::new("row_version", DataType::UInt64, false),
        text("collector"),
        text("peer_ip"),
        Field::new("peer_asn", DataType::UInt32, false),
        Field::new("generation", DataType::UInt64, false),
        Field::new("afi", DataType::UInt8, false),
        Field::new("safi", DataType::UInt8, false),
        text("baseline"),
        text("range_start"),
    ]));
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(UInt64Array::from(vec![part_seq; rows.len()])),
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.row_version).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.collector.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.peer_ip.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(UInt32Array::from(
                rows.iter().map(|r| r.peer_asn).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                rows.iter().map(|r| r.generation).collect::<Vec<_>>(),
            )),
            Arc::new(UInt8Array::from(
                rows.iter().map(|r| r.afi as u8).collect::<Vec<_>>(),
            )),
            Arc::new(UInt8Array::from(
                rows.iter().map(|r| r.safi as u8).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter().map(|r| r.baseline.clone()).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|r| r.range_start.clone())
                    .collect::<Vec<_>>(),
            )),
        ],
    )
    .map_err(|e| CatalogError::Parquet(e.to_string()))
}

/// The manifest naming the parts of the catalog generation a reader should trust.
pub const GENERATION: &str = "GENERATION";

/// The `(month directory, day prefix)` a stamp belongs to, taken from the row's own event time.
///
/// A late row therefore lands in the part for its own day instead of in whatever day the
/// writer happened to be in, which is the difference between a partition and a literal.
fn part_day(stamp: &str) -> Result<(String, String), CatalogError> {
    let seconds = stamp
        .split('.')
        .next()
        .and_then(|text| text.parse::<i64>().ok());
    let Some(seconds) = seconds else {
        return Err(CatalogError::Io {
            op: "read a row's event time",
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{stamp:?} is not <seconds>.<micros>"),
            ),
        });
    };
    let (year, month, day, _, _) = crate::civil::civil_from_unix(seconds);
    Ok((
        format!("{year:04}.{month:02}"),
        format!("{year:04}{month:02}{day:02}"),
    ))
}

/// The next free part number for one table and day, discovered rather than reserved.
fn next_part_seq(dir: &Path, day: &str) -> u64 {
    let prefix = format!("{day}.");
    let mut highest = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(rest) = name.strip_prefix(&prefix) else {
                continue;
            };
            let Some(number) = rest.strip_suffix(".parquet") else {
                continue;
            };
            if let Ok(value) = number.parse::<u64>() {
                highest = highest.max(value);
            }
        }
    }
    highest + 1
}

/// Write one immutable part and return its path.
///
/// The file is `catalog/<table>/<YYYY.MM>/<YYYYMMDD>.<NNNNNN>.parquet` and it is created with
/// `create_new`, so a part is never rewritten: a writer that loses a race for a part number
/// takes the next one instead of replacing rows a reader may be holding. Rows carry
/// `part_seq`, so a repeated key is resolved at read time rather than by one write erasing
/// the other.
fn write_part<F>(root: &Path, table: &str, stamp: &str, build: F) -> Result<PathBuf, CatalogError>
where
    F: Fn(u64) -> Result<RecordBatch, CatalogError>,
{
    let (month, day) = part_day(stamp)?;
    let dir = root.join("catalog").join(table).join(&month);
    std::fs::create_dir_all(&dir).map_err(|e| CatalogError::Io {
        op: "create a catalog table directory",
        source: e,
    })?;
    for _ in 0..8 {
        let seq = next_part_seq(&dir, &day);
        let path = dir.join(format!("{day}.{seq:06}.parquet"));
        let created = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        let file = match created {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(CatalogError::Io {
                    op: "create a parquet part",
                    source: e,
                })
            }
        };
        let batch = build(seq)?;
        let written = (|| {
            let mut writer = ArrowWriter::try_new(file, batch.schema(), None)
                .map_err(|e| CatalogError::Parquet(e.to_string()))?;
            writer
                .write(&batch)
                .map_err(|e| CatalogError::Parquet(e.to_string()))?;
            writer
                .close()
                .map_err(|e| CatalogError::Parquet(e.to_string()))?;
            Ok::<(), CatalogError>(())
        })();
        return match written {
            Ok(()) => Ok(path),
            Err(e) => {
                // A part that could not be finished must not stay on disk looking readable.
                let _ = std::fs::remove_file(&path);
                Err(e)
            }
        };
    }
    Err(CatalogError::Io {
        op: "allocate a catalog part number",
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("no free part number for {table} on {day}"),
        ),
    })
}

/// Write one part per day, grouping rows by the day their own event time falls in.
fn write_rows<T, F>(
    root: &Path,
    table: &str,
    rows: Vec<(String, T)>,
    build: F,
) -> Result<Vec<PathBuf>, CatalogError>
where
    F: Fn(&[T], u64) -> Result<RecordBatch, CatalogError>,
{
    let mut by_day: BTreeMap<(String, String), (String, Vec<T>)> = BTreeMap::new();
    for (stamp, row) in rows {
        let key = part_day(&stamp)?;
        by_day
            .entry(key)
            .or_insert_with(|| (stamp, Vec::new()))
            .1
            .push(row);
    }
    let mut written = Vec::new();
    for (_, (stamp, rows)) in by_day {
        written.push(write_part(root, table, &stamp, |seq| build(&rows, seq))?);
    }
    Ok(written)
}

/// Publish the generation manifest, replacing whatever generation was there.
///
/// A reader that reads this file first and then only the parts it names cannot see a rebuild
/// that died halfway: the manifest is the commit point, and no part is ever deleted, so
/// nothing a reader holds is rewritten underneath it.
///
/// A live append adds its part to the manifest instead of replacing it, so the manifest names
/// every part a reader should read rather than only the ones a rebuild wrote.
fn write_generation(root: &Path, parts: &[PathBuf]) -> Result<PathBuf, CatalogError> {
    let entries = describe_parts(root, parts)?;
    publish_generation(root, entries)
}

/// One manifest entry per part: its path relative to `catalog/`, its size and its hash.
fn describe_parts(root: &Path, parts: &[PathBuf]) -> Result<Vec<serde_json::Value>, CatalogError> {
    let catalog = root.join("catalog");
    let mut listed = Vec::new();
    for path in parts {
        let relative = path
            .strip_prefix(&catalog)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(path).map_err(|e| CatalogError::Io {
            op: "read a part back for the manifest",
            source: e,
        })?;
        listed.push(serde_json::json!({
            "path": relative,
            "bytes": bytes.len(),
            "sha256": crate::segment::sha256_hex(&bytes),
        }));
    }
    Ok(listed)
}

/// Write the manifest and make it visible with one rename.
fn publish_generation(
    root: &Path,
    listed: Vec<serde_json::Value>,
) -> Result<PathBuf, CatalogError> {
    let catalog = root.join("catalog");
    std::fs::create_dir_all(&catalog).map_err(|e| CatalogError::Io {
        op: "create the catalog directory",
        source: e,
    })?;
    let manifest = serde_json::json!({
        "schema_version": 1,
        "generation": crate::segment::render_stamp(crate::collector::now_micros()),
        "parts": listed,
    });
    let mut text = serde_json::to_string(&manifest)
        .map_err(|e| CatalogError::Parquet(format!("manifest: {e}")))?;
    text.push('\n');
    let temp = catalog.join("GENERATION.tmp");
    let final_path = catalog.join(GENERATION);
    let write = (|| {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    })();
    if let Err(e) = write {
        let _ = std::fs::remove_file(&temp);
        return Err(CatalogError::Io {
            op: "write the generation manifest",
            source: e,
        });
    }
    std::fs::rename(&temp, &final_path).map_err(|e| CatalogError::Io {
        op: "publish the generation manifest",
        source: e,
    })?;
    Ok(final_path)
}

fn coverage_logs(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join("coverage")) {
        for entry in entries.flatten() {
            let path = entry.path().join("updates.jsonl");
            if path.is_file() {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn sidecars(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if matches!(
            name.as_str(),
            "catalog"
                | "sessions"
                | "coverage"
                | "gaps"
                | "tombstones"
                | "work"
                | "quarantine"
                | "config"
                | "clients"
        ) {
            continue;
        }
        collect_sidecars(&path, &mut found);
    }
    found.sort();
    found
}

fn collect_sidecars(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sidecars(&path, found);
        } else if path.extension().is_some_and(|e| e == "json") {
            found.push(path);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bgpviews-catalog-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sidecar_for(collector: &str, range_start: &str) -> Sidecar {
        let mut parts = collector.splitn(2, '-');
        let client = parts.next().unwrap_or(collector).to_string();
        let view = parts.next().unwrap_or("adj-in-pre").to_string();
        Sidecar {
            schema_version: 1,
            collector: collector.to_string(),
            client,
            view,
            feed: "bmp".to_string(),
            stream: crate::segment::Stream::Updates,
            range_start: range_start.to_string(),
            range_end: range_start.to_string(),
            session_epoch: 7,
            ordinal: 1,
            path: format!("{collector}/bgpdata/2026.11/UPDATES/updates.20261115.1800.gz"),
            bytes: 42,
            records: 5,
            end_of_rib_records: 1,
            compression: "gzip".to_string(),
            sha256: "deadbeef".to_string(),
            state: "partial".to_string(),
            partial_reasons: vec!["session_ended".to_string()],
            late_arrival: 0,
            exporter_sys_name: None,
            exporter_sys_descr: None,
            first_router_time: None,
            last_router_time: None,
            baseline: vec![crate::segment::BaselineEntry {
                peer_ip: "172.31.250.11".to_string(),
                peer_asn: 65001,
                generation: 1,
                afi: 1,
                safi: 1,
                baseline: "unknown".to_string(),
                eor_journal_seq: None,
                eor_received_at: None,
            }],
        }
    }

    fn put_sidecar(dir: &Path, sidecar: &Sidecar) {
        let path = dir.join(&sidecar.path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not really gzip").unwrap();
        std::fs::write(
            format!("{}.json", path.display()),
            serde_json::to_string(sidecar).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn a_rebuild_writes_one_part_per_table_and_day_plus_a_manifest() {
        let dir = scratch("rebuild");
        put_sidecar(&dir, &sidecar_for("r2-adj-in-pre", "1763166000.000000"));
        let summary = rebuild(&dir).unwrap();
        assert_eq!(summary.segments, 1);
        assert_eq!(summary.sessions, 1);
        assert_eq!(summary.peers, 1);
        // A table with no rows gets no part: an empty part would claim a day it has nothing
        // to say about, and a reader would have to filter it out.
        assert_eq!(summary.files.len(), 3);
        for file in &summary.files {
            assert!(file.exists(), "{} must exist", file.display());
            let relative = file
                .strip_prefix(dir.join("catalog"))
                .unwrap()
                .to_string_lossy()
                .to_string();
            assert!(relative.contains("/2025.11/"), "{relative}");
            let name = file.file_name().unwrap().to_string_lossy().to_string();
            // 1763166000 is 2025-11-15, which is the point: the day comes from the stamp,
            // and the literal this test used to assert was wrong even for its own fixture.
            let (year, month, day, _, _) = crate::civil::civil_from_unix(1_763_166_000);
            assert!(
                name.starts_with(&format!("{year:04}{month:02}{day:02}.")),
                "{name}"
            );
            assert!(name.ends_with(".parquet"), "{name}");
            assert_eq!(name.len(), "20251115.000001.parquet".len(), "{name}");
        }
        let manifest = dir.join("catalog").join(GENERATION);
        assert!(manifest.exists(), "a rebuild must publish its generation");
        let text = std::fs::read_to_string(&manifest).unwrap();
        assert!(text.contains("segments/2025.11/20251115."), "{text}");
        assert!(text.ends_with('\n'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_day_comes_from_the_row_and_not_from_a_literal() {
        let dir = scratch("day");
        let stamp = format!("{}.000000", 1_800_000_000_i64);
        put_sidecar(&dir, &sidecar_for("r2-adj-in-pre", &stamp));
        let summary = rebuild(&dir).unwrap();
        let (year, month, day, _, _) = crate::civil::civil_from_unix(1_800_000_000);
        let names = summary
            .files
            .iter()
            .map(|file| file.display().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let expected_dir = format!("{year:04}.{month:02}");
        let expected_day = format!("{year:04}{month:02}{day:02}");
        assert!(names.contains(&expected_dir), "{names}");
        assert!(names.contains(&expected_day), "{names}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_rebuild_adds_parts_and_replaces_none() {
        let dir = scratch("again");
        put_sidecar(&dir, &sidecar_for("r2-adj-in-pre", "1763166000.000000"));
        let first = rebuild(&dir).unwrap();
        let second = rebuild(&dir).unwrap();
        for file in &first.files {
            assert!(
                file.exists(),
                "{} was removed by a second rebuild",
                file.display()
            );
            assert!(!second.files.contains(file), "a part was rewritten");
        }
        // One part per rebuild: the projection is append-only, and `part_seq` decides which
        // row a reader sees, so a rebuild never has to rewrite the part it replaces.
        let parts = std::fs::read_dir(dir.join("catalog/segments/2025.11"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "parquet"))
            .count();
        assert_eq!(parts, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sidecar_it_cannot_parse_fails_the_rebuild_and_leaves_the_manifest() {
        let dir = scratch("broken");
        put_sidecar(&dir, &sidecar_for("r2-adj-in-pre", "1763166000.000000"));
        rebuild(&dir).unwrap();
        let manifest = dir.join("catalog").join(GENERATION);
        let before = std::fs::read_to_string(&manifest).unwrap();

        let broken = dir.join("r2-adj-in-pre/bgpdata/2026.11/UPDATES/broken.json");
        std::fs::write(&broken, b"{ not json").unwrap();
        let result = rebuild(&dir);
        assert!(
            result.is_err(),
            "a sidecar it cannot read must fail the rebuild"
        );
        let after = std::fs::read_to_string(&manifest).unwrap();
        assert_eq!(
            before, after,
            "the manifest must not move on a failed rebuild"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_collectors_in_one_interval_get_two_parts() {
        let dir = scratch("two");
        let pre = sidecar_for("r2-adj-in-pre", "1763166000.000000");
        let post = sidecar_for("r2-adj-in-post", "1763166000.000000");
        let a = append_segment(&dir, &pre).unwrap();
        let b = append_segment(&dir, &post).unwrap();
        let segments = |paths: &[PathBuf]| {
            paths
                .iter()
                .filter(|path| path.display().to_string().contains("/segments/"))
                .cloned()
                .collect::<Vec<_>>()
        };
        let first = segments(&a);
        let second = segments(&b);
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_ne!(
            first[0], second[0],
            "one collector's row replaced the other's"
        );
        assert!(first[0].exists() && second[0].exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
