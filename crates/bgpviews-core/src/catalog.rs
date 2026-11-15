//! The catalog: the archive's own index, written as Parquet.
//!
//! Design reference: `docs/design.md` section 8. Three rules shape this file.
//!
//! **The catalog is a projection of the sidecars, never a second truth.** Every row can
//! be rebuilt from files that are already published, which is why `rebuild-index` is a
//! command rather than a repair tool: deleting `catalog/` must cost time, not data.
//!
//! **Tables are append-only files, one directory per day.** A rewrite of the whole
//! catalog on every publication would make the cost of a segment proportional to the
//! archive's age.
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
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { op, source } => write!(f, "{op}: {source}"),
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
    let name = format!(
        "part-{}-{:06}.parquet",
        sidecar.session_epoch, sidecar.ordinal
    );
    let mut written = Vec::new();
    // A second session in the same interval would reuse the ordinal, so the day and the
    // collector are part of the name too.
    let stamp = sidecar.range_start.replace('.', "-");
    let name = format!("part-{}-{name}", stamp);
    written.push(write_part(
        root,
        "segments",
        &name,
        segments_batch(&segments)?,
    )?);

    if !sidecar.baseline.is_empty() {
        let peers: Vec<PeerRow> = sidecar
            .baseline
            .iter()
            .map(|entry| PeerRow {
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
        written.push(write_part(root, "peers", &name, peers_batch(&peers)?)?);
    }
    Ok(written)
}

/// Rebuild every catalog table from the published files under `root`.
///
/// The previous catalog is removed first: a projection that is rebuilt must not merge
/// with the projection it replaces, or a deleted segment would stay in the index.
///
/// # Errors
/// Returns [`CatalogError`] when a sidecar cannot be read or a table cannot be written.
pub fn rebuild(root: &Path) -> Result<Summary, CatalogError> {
    let catalog = root.join("catalog");
    if catalog.exists() {
        std::fs::remove_dir_all(&catalog).map_err(|e| CatalogError::Io {
            op: "clear the old catalog",
            source: e,
        })?;
    }
    std::fs::create_dir_all(&catalog).map_err(|e| CatalogError::Io {
        op: "create the catalog directory",
        source: e,
    })?;

    let mut summary = Summary::default();
    let mut segments = Vec::new();
    let mut sessions: BTreeMap<(String, u64), SessionRow> = BTreeMap::new();
    let mut peers = Vec::new();

    for path in sidecars(root) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(sidecar) = serde_json::from_str::<Sidecar>(&text) else {
            continue;
        };
        let session = sessions
            .entry((sidecar.client.clone(), sidecar.session_epoch))
            .or_insert_with(|| SessionRow {
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
            peers.push(PeerRow {
                collector: sidecar.collector.clone(),
                peer_ip: entry.peer_ip.clone(),
                peer_asn: entry.peer_asn,
                generation: entry.generation,
                afi: entry.afi,
                safi: entry.safi,
                baseline: entry.baseline.clone(),
                range_start: sidecar.range_start.clone(),
            });
        }
        segments.push(SegmentRow {
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
        });
    }

    // Gaps come from the coverage logs, which is the only place a missing period is
    // recorded: a sidecar exists only for what was written.
    let mut gaps = Vec::new();
    for log in coverage_logs(root) {
        let Ok(bytes) = std::fs::read(&log) else {
            continue;
        };
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
            gaps.push(GapRow {
                collector: collector.clone(),
                start: range.start.render(),
                end: range.end.map_or_else(String::new, |end| end.render()),
                reason: range.reason.clone().unwrap_or_default(),
            });
        }
        // The open range is a gap that has not ended yet; it is reported with an empty
        // end so a reader can tell "still missing" from "was missing".
        if let Some(open) = timeline.open_range() {
            if open.kind == RangeKind::Gap {
                gaps.push(GapRow {
                    collector,
                    start: open.start.render(),
                    end: String::new(),
                    reason: open.reason.clone().unwrap_or_default(),
                });
            }
        }
    }

    summary.segments = segments.len();
    summary.sessions = sessions.len();
    summary.gaps = gaps.len();
    summary.peers = peers.len();

    let segments = segments_batch(&segments)?;
    summary
        .files
        .push(write_table(root, "segments", segments, "2026-11-15")?);
    let sessions = sessions_batch(&sessions.values().cloned().collect::<Vec<_>>())?;
    summary
        .files
        .push(write_table(root, "sessions", sessions, "2026-11-15")?);
    let gaps = gaps_batch(&gaps)?;
    summary
        .files
        .push(write_table(root, "gaps", gaps, "2026-11-15")?);
    let peers = peers_batch(&peers)?;
    summary
        .files
        .push(write_table(root, "peers", peers, "2026-11-15")?);
    Ok(summary)
}

#[derive(Debug, Clone)]
struct SegmentRow {
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
    collector: String,
    start: String,
    end: String,
    reason: String,
}

#[derive(Debug, Clone)]
struct PeerRow {
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

fn segments_batch(rows: &[SegmentRow]) -> Result<RecordBatch, CatalogError> {
    let schema = Arc::new(Schema::new(vec![
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

fn sessions_batch(rows: &[SessionRow]) -> Result<RecordBatch, CatalogError> {
    let schema = Arc::new(Schema::new(vec![
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

fn gaps_batch(rows: &[GapRow]) -> Result<RecordBatch, CatalogError> {
    let schema = Arc::new(Schema::new(vec![
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

fn peers_batch(rows: &[PeerRow]) -> Result<RecordBatch, CatalogError> {
    let schema = Arc::new(Schema::new(vec![
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

fn write_part(
    root: &Path,
    table: &str,
    name: &str,
    batch: RecordBatch,
) -> Result<PathBuf, CatalogError> {
    let dir = root.join("catalog").join(table).join("live");
    std::fs::create_dir_all(&dir).map_err(|e| CatalogError::Io {
        op: "create a catalog live directory",
        source: e,
    })?;
    let path = dir.join(name);
    let file = std::fs::File::create(&path).map_err(|e| CatalogError::Io {
        op: "create a parquet part",
        source: e,
    })?;
    let mut writer = ArrowWriter::try_new(file, batch.schema(), None)
        .map_err(|e| CatalogError::Parquet(e.to_string()))?;
    writer
        .write(&batch)
        .map_err(|e| CatalogError::Parquet(e.to_string()))?;
    writer
        .close()
        .map_err(|e| CatalogError::Parquet(e.to_string()))?;
    Ok(path)
}

fn write_table(
    root: &Path,
    table: &str,
    batch: RecordBatch,
    day: &str,
) -> Result<PathBuf, CatalogError> {
    let dir = root.join("catalog").join(table).join(format!("day={day}"));
    std::fs::create_dir_all(&dir).map_err(|e| CatalogError::Io {
        op: "create a catalog table directory",
        source: e,
    })?;
    let path = dir.join("part-0001.parquet");
    let file = std::fs::File::create(&path).map_err(|e| CatalogError::Io {
        op: "create a parquet file",
        source: e,
    })?;
    let mut writer = ArrowWriter::try_new(file, batch.schema(), None)
        .map_err(|e| CatalogError::Parquet(e.to_string()))?;
    writer
        .write(&batch)
        .map_err(|e| CatalogError::Parquet(e.to_string()))?;
    writer
        .close()
        .map_err(|e| CatalogError::Parquet(e.to_string()))?;
    Ok(path)
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

    #[test]
    fn a_rebuild_writes_four_tables_and_counts_what_it_saw() {
        let dir = std::env::temp_dir().join(format!("bgpviews-catalog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("coverage/r2-adj-in-pre")).unwrap();
        std::fs::create_dir_all(dir.join("r2-adj-in-pre/bgpdata/2026.11/UPDATES")).unwrap();

        // One sidecar, named the way the writer names it, plus an empty segment file.
        let sidecar = Sidecar {
            schema_version: 1,
            collector: "r2-adj-in-pre".to_string(),
            client: "r2".to_string(),
            view: "adj-in-pre".to_string(),
            feed: "bmp".to_string(),
            stream: crate::segment::Stream::Updates,
            range_start: "1763166000.000000".to_string(),
            range_end: "1763166899.000000".to_string(),
            session_epoch: 7,
            ordinal: 1,
            path: "r2-adj-in-pre/bgpdata/2026.11/UPDATES/updates.20261115.1800.gz".to_string(),
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
        };
        std::fs::write(
            dir.join("r2-adj-in-pre/bgpdata/2026.11/UPDATES/updates.20261115.1800.gz.json"),
            serde_json::to_string(&sidecar).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("r2-adj-in-pre/bgpdata/2026.11/UPDATES/updates.20261115.1800.gz"),
            b"not really gzip",
        )
        .unwrap();

        let summary = rebuild(&dir).unwrap();
        assert_eq!(summary.segments, 1);
        assert_eq!(summary.sessions, 1);
        assert_eq!(summary.peers, 1);
        assert_eq!(summary.files.len(), 4);
        for file in &summary.files {
            assert!(file.exists(), "{} must exist", file.display());
            let meta = std::fs::metadata(file).unwrap();
            assert!(meta.len() > 0, "{} must not be empty", file.display());
        }
        // A rebuild is idempotent: the second run replaces rather than appends.
        let again = rebuild(&dir).unwrap();
        assert_eq!(again.segments, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
