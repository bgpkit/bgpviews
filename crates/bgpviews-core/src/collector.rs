//! The collector: one session, its journal, its coverage timeline, its segments.
//!
//! This is the single implementation of the state machine. The live path (`run`)
//! feeds it frames from a socket and the replay harness feeds it frames from a
//! capture, so a difference between the two can only be in how bytes arrive, never
//! in what the archive means.
//!
//! Ownership rules this type enforces, from `docs/design.md`:
//!
//! - The journal is written and synced before anything derives from a frame.
//! - A segment is published by rename, and its coverage line is written before the
//!   publication that depends on it.
//! - The timeline has exactly one open range at all times.

use std::collections::{BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::io::Write;
use std::net::IpAddr;
use std::path::PathBuf;

use crate::bgp4mp::{encode_record, RecordContext};
use crate::bmp::{FrameError, FrameKind, StreamSplitter};
use crate::coverage::{encode_line, Line, RangeKind, SegmentRef, Stamp, Timeline};
use crate::journal::{JournalError, JournalWriter};
use crate::rib::{frame_records_into, Rib, RibPeer};
use crate::segment::{render_stamp, BaselineEntry, SegmentError, SegmentWriter, Sidecar, Stream};

/// How a collector is configured.
#[derive(Debug, Clone)]
pub struct CollectorConfig {
    /// Configured client name.
    pub client: String,
    /// The view this collector records.
    pub view: String,
    /// Session epoch.
    pub epoch: u64,
    /// Interval length in seconds. Whole minutes only, because segment names have
    /// minute resolution.
    pub interval_seconds: i64,
    /// Archive root.
    pub root: PathBuf,
    /// Whether to write a RIB at every interval boundary.
    pub rib: bool,
    /// Used percentage at which RIB writing stops.
    pub disk_warning_percent: u8,
    /// Used percentage at which the session closes.
    pub disk_critical_percent: u8,
}

impl CollectorConfig {
    /// The collector name, `<client>-<view>`.
    #[must_use]
    pub fn collector(&self) -> String {
        format!("{}-{}", self.client, self.view)
    }
}

/// Why a collector could not do its job.
#[derive(Debug)]
pub enum CollectorError {
    /// A frame could not be classified.
    Frame(FrameError),
    /// The journal failed.
    Journal(JournalError),
    /// A segment failed.
    Segment(SegmentError),
    /// The coverage log failed.
    Coverage(String),
    /// An I/O error with the operation that failed.
    Io {
        /// What was being attempted.
        op: &'static str,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The archive's filesystem reached the critical threshold: the session closed rather
    /// than write half a record.
    DiskCritical,
    /// The interval is not a whole number of minutes.
    InvalidInterval {
        /// The interval supplied, in seconds.
        seconds: i64,
    },
    /// A route record arrived before the Peer Up that gives its envelope its local
    /// AS and address.
    MissingPeerUp {
        /// The peer address.
        peer: IpAddr,
    },
}

impl std::fmt::Display for CollectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame(e) => write!(f, "{e}"),
            Self::Journal(e) => write!(f, "{e}"),
            Self::Segment(e) => write!(f, "{e}"),
            Self::Coverage(detail) => write!(f, "coverage: {detail}"),
            Self::DiskCritical => write!(
                f,
                "the archive's filesystem reached the critical threshold; the session closed"
            ),
            Self::Io { op, source } => write!(f, "{op} failed: {source}"),
            Self::InvalidInterval { seconds } => write!(
                f,
                "interval {seconds}s is not a whole number of minutes; segment names would collide"
            ),
            Self::MissingPeerUp { peer } => {
                write!(f, "route record for peer {peer} arrived before its Peer Up")
            }
        }
    }
}

impl std::error::Error for CollectorError {}

impl From<FrameError> for CollectorError {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

impl From<JournalError> for CollectorError {
    fn from(value: JournalError) -> Self {
        Self::Journal(value)
    }
}

impl From<SegmentError> for CollectorError {
    fn from(value: SegmentError) -> Self {
        Self::Segment(value)
    }
}

/// One record from a BGP feed, which needs no BMP per-peer header: everything the
/// envelope needs is either configured or in the OPEN exchange.
#[derive(Debug, Clone)]
pub struct BgpRecord {
    /// The peer's address.
    pub peer_ip: IpAddr,
    /// The peer's AS.
    pub peer_asn: u32,
    /// Our AS.
    pub local_asn: u32,
    /// Our address.
    pub local_ip: IpAddr,
    /// Width of the AS numbers in the UPDATE.
    pub asn_len: bgpkit_parser::models::AsnLength,
    /// Whether this UPDATE is an end-of-RIB marker.
    pub is_eor: bool,
    /// Families the marker closes, when it is one.
    pub eor_families: Vec<(u16, u16)>,
}

/// One published segment.
#[derive(Debug, Clone)]
pub struct PublishedSegment {
    /// The published MRT path.
    pub path: PathBuf,
    /// Its sidecar path.
    pub sidecar_path: PathBuf,
    /// The sidecar as written.
    pub sidecar: Sidecar,
    /// The interval this segment belongs to, in microseconds.
    pub interval_start: i64,
}

#[derive(Debug, Clone, Copy)]
struct Peer {
    peer_asn: u32,
    local_asn: u32,
    local_ip: IpAddr,
    generation: u64,
}

struct Open {
    writer: SegmentWriter,
    interval_start: i64,
    ordinal: u32,
    reasons: BTreeSet<String>,
    eor: u64,
    first: i64,
    last: i64,
    first_router: Option<i64>,
    last_router: Option<i64>,
}

/// A collector's live state.
pub struct Collector {
    config: CollectorConfig,
    journal: JournalWriter,
    coverage: std::fs::File,
    timeline: Timeline,
    peers: HashMap<IpAddr, Peer>,
    /// First EoR per `(peer, generation, family)`, and nothing after it.
    baseline: HashMap<(IpAddr, u64, u16, u16), BaselineEntry>,
    open: Option<Open>,
    ordinals: HashMap<i64, u32>,
    exporter_name: Option<String>,
    exporter_descr: Option<String>,
    published: Vec<PublishedSegment>,
    records: u64,
    end_of_rib: u64,
    /// Every frame accepted, including non-routing ones.
    frames: u64,
    first_segment_at: Option<i64>,
    last_frame_at: Option<i64>,
    /// Frames whose view did not match this collector, counted not archived.
    unconfigured_view: u64,
    /// The Adj-RIB-In this collector keeps, when RIB writing is enabled.
    rib: Rib,
    /// The disk level seen at the last check, for reporting and for the RIB gate.
    disk_level: crate::disk::Level,
    /// RIB snapshots written, kept apart from update segments because they answer a
    /// different question: `published` is the record of what arrived, this is the
    /// record of what the collector believed at each boundary.
    ribs: Vec<PublishedSegment>,
}

impl Collector {
    /// Create a collector whose timeline starts now.
    ///
    /// # Errors
    /// As [`Collector::open_at`].
    pub fn open(config: CollectorConfig) -> Result<Self, CollectorError> {
        Self::open_at(config, now_micros())
    }

    /// Create a collector whose timeline starts at `start_micros`.
    ///
    /// The start is a parameter rather than a clock read inside, because a
    /// collector has exactly one time base and the caller owns it: the live path
    /// passes the system clock once, and a replay passes the capture's time.
    ///
    /// # Errors
    /// Returns [`CollectorError`] when the interval is not a whole number of
    /// minutes or a file cannot be created.
    pub fn open_at(config: CollectorConfig, start_micros: i64) -> Result<Self, CollectorError> {
        if config.interval_seconds < 60 || config.interval_seconds % 60 != 0 {
            return Err(CollectorError::InvalidInterval {
                seconds: config.interval_seconds,
            });
        }
        let collector = config.collector();
        let journal_path = config.root.join(format!(
            "sessions/{}/{:016}/journal.0.bin",
            config.client, config.epoch
        ));
        let journal = JournalWriter::create(&journal_path)?;

        let coverage_path = config
            .root
            .join(format!("coverage/{collector}/updates.jsonl"));
        if let Some(parent) = coverage_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CollectorError::Io {
                op: "create coverage directory",
                source: e,
            })?;
        }
        let mut coverage = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&coverage_path)
            .map_err(|e| CollectorError::Io {
                op: "open coverage log",
                source: e,
            })?;

        // A collector always starts as a gap: nothing has been written yet, and a
        // segment range has to name an artifact it does not have.
        // The coverage log is append-only for the collector's whole life, not per
        // session: a second session continues the same timeline instead of opening a
        // new one, which would make two `OPEN` lines and break the partition.
        let existing = std::fs::read(&coverage_path).unwrap_or_default();
        let timeline = if existing.is_empty() {
            let mut timeline = Timeline::default();
            let line = Line::Open {
                at: stamp(start_micros)?,
                range_kind: RangeKind::Gap,
                reason: Some("no_session".to_string()),
            };
            timeline
                .apply(&line)
                .map_err(|e| CollectorError::Coverage(e.to_string()))?;
            coverage
                .write_all(encode_line(&line).as_bytes())
                .and_then(|()| coverage.sync_data())
                .map_err(|e| CollectorError::Io {
                    op: "write coverage line",
                    source: e,
                })?;
            timeline
        } else {
            let (timeline, summary) = Timeline::replay(&existing);
            if summary.discarded_lines > 0 {
                return Err(CollectorError::Coverage(format!(
                    "coverage log has {} discarded line(s); run verify",
                    summary.discarded_lines
                )));
            }
            timeline
        };

        Ok(Self {
            config,
            journal,
            coverage,
            timeline,
            peers: HashMap::new(),
            baseline: HashMap::new(),
            open: None,
            ordinals: HashMap::new(),
            exporter_name: None,
            exporter_descr: None,
            published: Vec::new(),
            records: 0,
            end_of_rib: 0,
            frames: 0,
            first_segment_at: None,
            last_frame_at: None,
            unconfigured_view: 0,
            rib: Rib::new(),
            disk_level: crate::disk::Level::Ok,
            ribs: Vec::new(),
        })
    }

    /// Accept one message from a BGP feed.
    ///
    /// The record takes the same path as a BMP Route Monitoring frame: journal, then
    /// segment placement, then the envelope. What differs is where the context comes
    /// from, and that the end-of-RIB here is the peer's own marker rather than the
    /// exporter's dump boundary.
    ///
    /// # Errors
    /// Returns [`CollectorError`] when the record cannot be journaled or placed.
    pub fn on_bgp_update(
        &mut self,
        record: BgpRecord,
        raw_message: &[u8],
        at: i64,
    ) -> Result<(), CollectorError> {
        self.frames += 1;
        self.last_frame_at = Some(at);
        // A BGP feed has no Peer Up message, so the session itself is the generation:
        // the peer exists for as long as this connection does.
        self.peers.entry(record.peer_ip).or_insert(Peer {
            peer_asn: record.peer_asn,
            local_asn: record.local_asn,
            local_ip: record.local_ip,
            generation: 1,
        });
        self.journal.append(
            at.div_euclid(1_000_000),
            at.rem_euclid(1_000_000) as u32,
            raw_message,
        )?;
        self.journal.sync()?;

        if self.config.rib && !record.is_eor {
            self.rib_apply(record.peer_ip, record.peer_asn, raw_message, at);
        }

        let interval_start = at.div_euclid(self.config.interval_seconds * 1_000_000)
            * self.config.interval_seconds
            * 1_000_000;
        let needs_new = self
            .open
            .as_ref()
            .is_none_or(|current| current.interval_start != interval_start);
        if needs_new {
            if let Some(current) = self.open.take() {
                self.publish(current)?;
            }
            let ordinal = {
                let slot = self.ordinals.entry(interval_start).or_insert(0);
                *slot += 1;
                *slot
            };
            let path = segment_path(
                &self.config.collector(),
                interval_start,
                self.config.interval_seconds,
                ordinal,
            );
            let writer = SegmentWriter::create(
                &self.config.root,
                &self.config.collector(),
                Stream::Updates,
                &path,
            )?;
            if self.first_segment_at.is_none() {
                self.first_segment_at = Some(at);
            }
            self.coverage_segment(at, ordinal)?;
            self.open = Some(Open {
                writer,
                interval_start,
                ordinal,
                reasons: BTreeSet::new(),
                eor: 0,
                first: at,
                last: at,
                first_router: None,
                last_router: None,
            });
        }

        let current = self.open.as_mut().ok_or_else(|| {
            CollectorError::Coverage("a record arrived with no open segment".to_string())
        })?;
        let context = RecordContext {
            seconds: at.div_euclid(1_000_000) as u32,
            micros: at.rem_euclid(1_000_000) as u32,
            peer_asn: record.peer_asn,
            local_asn: record.local_asn,
            peer_ip: record.peer_ip,
            local_ip: record.local_ip,
            interface_index: 0,
            asn_len: record.asn_len,
        };
        let envelope = encode_record(&context, raw_message)
            .map_err(|e| CollectorError::Coverage(e.to_string()))?;
        current.writer.append(&envelope, at)?;
        current.last = at;
        self.records += 1;
        if record.is_eor {
            current.eor += 1;
            self.end_of_rib += 1;
            let generation = 1;
            for (afi, safi) in record.eor_families {
                self.baseline
                    .entry((record.peer_ip, generation, afi, safi))
                    .or_insert_with(|| BaselineEntry {
                        peer_ip: record.peer_ip.to_string(),
                        peer_asn: record.peer_asn,
                        generation,
                        afi,
                        safi,
                        baseline: "observed".to_string(),
                        eor_received_at: Some(render_stamp(at)),
                        eor_journal_seq: Some(self.frames),
                    });
            }
        }
        Ok(())
    }

    /// Accept one complete frame.
    ///
    /// # Errors
    /// Returns [`CollectorError`] when the frame cannot be classified, the peer is
    /// unknown, or an artifact cannot be written.
    pub fn on_frame(&mut self, bytes: &[u8], at: i64) -> Result<(), CollectorError> {
        let kind = crate::bmp::classify_frame(bytes)?;
        self.frames += 1;
        self.last_frame_at = Some(at);

        // Journal first: nothing downstream may see a frame the journal lacks.
        self.journal.append(
            at.div_euclid(1_000_000),
            at.rem_euclid(1_000_000) as u32,
            bytes,
        )?;
        self.journal.sync()?;

        match kind {
            FrameKind::Initiation {
                sys_name,
                sys_descr,
            } => {
                if self.exporter_name.is_none() {
                    self.exporter_name.clone_from(&sys_name);
                }
                if self.exporter_descr.is_none() {
                    self.exporter_descr.clone_from(&sys_descr);
                }
            }
            FrameKind::PeerUp {
                peer_ip,
                peer_asn,
                local_ip,
                local_asn,
                ..
            } => {
                let generation = self.peers.get(&peer_ip).map_or(1, |p| p.generation + 1);
                let restarted = self.peers.insert(
                    peer_ip,
                    Peer {
                        peer_asn,
                        local_asn,
                        local_ip,
                        generation,
                    },
                );
                if restarted.is_some() {
                    if let Some(current) = self.open.take() {
                        let mut current = current;
                        current.reasons.insert("peer_restarted".to_string());
                        self.publish(current)?;
                    }
                }
            }
            FrameKind::PeerDown { .. } => {
                if let Some(current) = self.open.as_mut() {
                    current.reasons.insert("peer_vanished".to_string());
                }
            }
            FrameKind::RouteMonitoring {
                peer_ip,
                asn_len,
                is_eor,
                eor_family,
                router_time,
                pdu_offset,
                post_policy,
                adj_rib_out,
                ..
            } => {
                // v1 archives Adj-RIB-In pre-policy only. Anything else is counted
                // rather than written into a collector it does not belong to.
                if post_policy || adj_rib_out {
                    self.unconfigured_view += 1;
                    return Ok(());
                }
                let Some(peer) = self.peers.get(&peer_ip).copied() else {
                    return Err(CollectorError::MissingPeerUp { peer: peer_ip });
                };
                if self.config.rib && !is_eor {
                    self.rib_apply(peer_ip, peer.peer_asn, &bytes[pdu_offset..], at);
                }
                let interval_start = at.div_euclid(self.config.interval_seconds * 1_000_000)
                    * self.config.interval_seconds
                    * 1_000_000;

                let needs_new = self
                    .open
                    .as_ref()
                    .is_none_or(|current| current.interval_start != interval_start);
                if needs_new {
                    if let Some(current) = self.open.take() {
                        self.publish(current)?;
                    }
                    let ordinal = self.next_ordinal(interval_start);
                    let path = segment_path(
                        &self.config.collector(),
                        interval_start,
                        self.config.interval_seconds,
                        ordinal,
                    );
                    let writer = SegmentWriter::create(
                        &self.config.root,
                        &self.config.collector(),
                        Stream::Updates,
                        &path,
                    )?;
                    if self.first_segment_at.is_none() {
                        self.first_segment_at = Some(at);
                    }
                    self.coverage_segment(at, ordinal)?;
                    self.open = Some(Open {
                        writer,
                        interval_start,
                        ordinal,
                        reasons: BTreeSet::new(),
                        eor: 0,
                        first: at,
                        last: at,
                        first_router: None,
                        last_router: None,
                    });
                }

                let current = self.open.as_mut().ok_or_else(|| {
                    CollectorError::Coverage("a record arrived with no open segment".to_string())
                })?;
                let context = RecordContext {
                    seconds: at.div_euclid(1_000_000) as u32,
                    micros: at.rem_euclid(1_000_000) as u32,
                    peer_asn: peer.peer_asn,
                    local_asn: peer.local_asn,
                    peer_ip,
                    local_ip: peer.local_ip,
                    interface_index: 0,
                    asn_len,
                };
                let pdu = &bytes[pdu_offset..];
                let record = encode_record(&context, pdu)
                    .map_err(|e| CollectorError::Coverage(e.to_string()))?;
                current.writer.append(&record, at)?;
                current.last = at;
                self.records += 1;
                if is_eor {
                    current.eor += 1;
                    self.end_of_rib += 1;
                }
                if let Some(router) = router_time {
                    let router_micros = (router * 1_000_000.0) as i64;
                    current.first_router.get_or_insert(router_micros);
                    current.last_router = Some(router_micros);
                }
                // The initial marker for this object is baseline evidence; later
                // ones are ignored, so a refresh cannot rewrite what we recorded.
                if let Some((afi, safi)) = eor_family {
                    let key = (peer_ip, peer.generation, afi, safi);
                    self.baseline.entry(key).or_insert_with(|| BaselineEntry {
                        peer_ip: peer_ip.to_string(),
                        peer_asn: peer.peer_asn,
                        generation: peer.generation,
                        afi,
                        safi,
                        baseline: "observed".to_string(),
                        eor_received_at: Some(render_stamp(at)),
                        eor_journal_seq: Some(self.frames),
                    });
                }
            }
            FrameKind::Other { .. } => {}
        }
        Ok(())
    }

    /// Close the session: publish the open segment as partial and close the gap.
    ///
    /// # Errors
    /// Returns [`CollectorError`] when an artifact cannot be written.
    pub fn finish(&mut self, at: i64) -> Result<(), CollectorError> {
        if let Some(mut current) = self.open.take() {
            current.reasons.insert("session_ended".to_string());
            self.publish(current)?;
        }
        self.coverage_gap(
            at.max(self.last_frame_at.unwrap_or(at)) + 1,
            "session_ended",
        )?;
        self.journal.sync()?;
        Ok(())
    }

    /// Published segments, in order.
    #[must_use]
    pub fn published(&self) -> &[PublishedSegment] {
        &self.published
    }

    /// The disk level as of the last check.
    #[must_use]
    pub fn disk_level(&self) -> crate::disk::Level {
        self.disk_level
    }

    /// Measure the archive's filesystem and remember the result.
    ///
    /// Returns the level so a caller can act on it; the collector uses it to gate RIB
    /// writing, which is the first thing to give up when the disk fills.
    pub fn check_disk(&mut self) -> crate::disk::Level {
        let warning = self.config.disk_warning_percent;
        let critical = self.config.disk_critical_percent;
        self.disk_level = match crate::disk::used_percent(&self.config.root) {
            Some(percent) => crate::disk::Level::from_percent(percent, warning, critical),
            // Unmeasurable is not a reason to close a session.
            None => crate::disk::Level::Ok,
        };
        self.disk_level
    }

    /// Whether this collector writes RIB snapshots.
    #[must_use]
    pub fn rib_enabled(&self) -> bool {
        self.config.rib
    }

    /// Turn RIB writing on or off, as a reloaded configuration may ask.
    pub fn set_rib_enabled(&mut self, enabled: bool) {
        self.config.rib = enabled;
    }

    /// The interval this collector was opened with, in seconds.
    #[must_use]
    pub fn interval_seconds(&self) -> i64 {
        self.config.interval_seconds
    }

    /// RIB snapshots written, in order.
    #[must_use]
    pub fn rib_snapshots(&self) -> &[PublishedSegment] {
        &self.ribs
    }

    /// The in-memory RIB, for reporting.
    #[must_use]
    pub fn rib(&self) -> &Rib {
        &self.rib
    }

    /// Frames accepted.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// MRT records written.
    #[must_use]
    pub fn records(&self) -> u64 {
        self.records
    }

    /// End-of-RIB markers written.
    #[must_use]
    pub fn end_of_rib(&self) -> u64 {
        self.end_of_rib
    }

    /// Frames for a view this collector does not archive.
    #[must_use]
    pub fn unconfigured_view(&self) -> u64 {
        self.unconfigured_view
    }

    /// The journal part this collector writes.
    #[must_use]
    pub fn journal_path(&self) -> PathBuf {
        self.journal.path().to_path_buf()
    }

    /// The coverage log this collector writes.
    #[must_use]
    pub fn coverage_path(&self) -> PathBuf {
        self.config.root.join(format!(
            "coverage/{}/updates.jsonl",
            self.config.collector()
        ))
    }

    /// The next ordinal for an interval, from the files already on disk.
    ///
    /// Ordinals are per `(collector, stream, interval_start)` for the collector's
    /// whole life, so a second session in the same interval must not reuse ordinal 1
    /// and overwrite the first session's segment. Scanning is the only source that
    /// survives a restart.
    fn next_ordinal(&self, interval_start: i64) -> u32 {
        let name = interval_name(interval_start, self.config.interval_seconds);
        let dir = self.config.root.join(format!(
            "{}/bgpdata/{}/{}",
            self.config.collector(),
            name.month,
            Stream::Updates.directory()
        ));
        let mut highest = 0u32;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let file = entry.file_name().to_string_lossy().to_string();
                if !file.starts_with(&format!("{}.", name.file)) {
                    continue;
                }
                let rest = &file[name.file.len() + 1..];
                let ordinal = match rest.split('.').next() {
                    Some("gz") => 1,
                    Some(number) => number.parse::<u32>().unwrap_or(0),
                    None => 0,
                };
                highest = highest.max(ordinal);
            }
        }
        highest + 1
    }

    /// Baseline rows for the generations currently known, sorted so the sidecar is
    /// reproducible. A peer with no observed marker gets `unknown`, which is the
    /// honest state rather than an omission.
    fn baseline_rows(&self) -> Vec<BaselineEntry> {
        let mut rows: Vec<BaselineEntry> = Vec::new();
        let mut peers: Vec<(&IpAddr, &Peer)> = self.peers.iter().collect();
        peers.sort_by_key(|(ip, _)| **ip);
        for (ip, peer) in peers {
            for (afi, safi) in [(1u16, 1u16), (2, 1)] {
                let known = self
                    .baseline
                    .get(&(*ip, peer.generation, afi, safi))
                    .cloned();
                rows.push(known.unwrap_or(BaselineEntry {
                    peer_ip: ip.to_string(),
                    peer_asn: peer.peer_asn,
                    generation: peer.generation,
                    afi,
                    safi,
                    baseline: "unknown".to_string(),
                    eor_received_at: None,
                    eor_journal_seq: None,
                }));
            }
        }
        rows
    }

    fn coverage_segment(&mut self, at: i64, ordinal: u32) -> Result<(), CollectorError> {
        let line = Line::Transition {
            at: stamp(at)?,
            range_kind: RangeKind::Segment,
            reason: None,
            segment: Some(SegmentRef {
                session_epoch: self.config.epoch,
                ordinal,
            }),
            carried_reasons: Vec::new(),
            carried_through_seq: None,
        };
        self.append_coverage(line)
    }

    fn coverage_gap(&mut self, at: i64, reason: &str) -> Result<(), CollectorError> {
        let line = Line::Transition {
            at: stamp(at)?,
            range_kind: RangeKind::Gap,
            reason: Some(reason.to_string()),
            segment: None,
            carried_reasons: Vec::new(),
            carried_through_seq: None,
        };
        self.append_coverage(line)
    }

    fn append_coverage(&mut self, line: Line) -> Result<(), CollectorError> {
        // A transition closes whatever was open at that instant. If that was a gap, its
        // end is now known, so the catalog can hold the row without re-deriving anything.
        if let (Line::Transition { at, .. }, Some(open)) = (&line, self.timeline.open_range()) {
            if open.kind == RangeKind::Gap {
                if let Some(reason) = open.reason.clone() {
                    let start = open.start.render();
                    let end = at.render();
                    if let Err(e) = crate::catalog::append_gap(
                        &self.config.root,
                        &self.config.collector(),
                        &start,
                        &end,
                        &reason,
                    ) {
                        eprintln!("bgpviews: catalog gap append failed: {e}");
                    }
                }
            }
        }
        self.timeline
            .apply(&line)
            .map_err(|e| CollectorError::Coverage(e.to_string()))?;
        self.coverage
            .write_all(encode_line(&line).as_bytes())
            .and_then(|()| self.coverage.sync_data())
            .map_err(|e| CollectorError::Io {
                op: "write coverage line",
                source: e,
            })
    }

    /// Apply one UPDATE to the in-memory RIB, counting refusals instead of failing.
    ///
    /// A message the RIB cannot read is a fact about the message, not a reason to stop
    /// archiving: the update segment already holds it verbatim, so the RIB stays honest
    /// about what it does not cover.
    fn rib_apply(&mut self, peer_ip: IpAddr, peer_asn: u32, update: &[u8], at: i64) {
        let peer = RibPeer {
            ip: peer_ip,
            asn: peer_asn,
            bgp_id: std::net::Ipv4Addr::UNSPECIFIED,
        };
        let seconds = at.div_euclid(1_000_000);
        let seconds = u32::try_from(seconds).unwrap_or(u32::MAX);
        if self.rib.apply(peer, update, seconds).is_err() {
            self.rib.note_refused();
        }
    }

    /// Write a RIB now, because something outside the interval clock asked for one.
    ///
    /// SIGHUP is the reason this exists. The snapshot inherits the reasons of the
    /// interval that is currently open, so an on-demand RIB is no more certain than the
    /// traffic it summarizes.
    ///
    /// # Errors
    /// Returns [`CollectorError`] when the snapshot cannot be written.
    pub fn snapshot_rib(&mut self, at: i64) -> Result<(), CollectorError> {
        let reasons: Vec<String> = self
            .open
            .as_ref()
            .map(|open| open.reasons.iter().cloned().collect())
            .unwrap_or_default();
        self.write_rib(at, &reasons)
    }

    /// Write the RIB as of `boundary_micros`, named `ribs.<YYYYMMDD.HHMM>.gz`.
    ///
    /// The RIB inherits the caveats of the interval it summarizes: if that interval was
    /// partial for named reasons, so is the snapshot, because a RIB cannot be more
    /// certain than the updates behind it.
    fn write_rib(
        &mut self,
        boundary_micros: i64,
        reasons: &[String],
    ) -> Result<(), CollectorError> {
        // The derived view goes first: a RIB can be recomputed from the update stream,
        // and an update that was never written cannot.
        if self.check_disk().skip_ribs() {
            eprintln!(
                "bgpviews: disk {}%; skipping the RIB at {}",
                crate::disk::used_percent(&self.config.root).unwrap_or(0),
                render_stamp(boundary_micros)
            );
            return Ok(());
        }
        let collector = self.config.collector();
        let secs = boundary_micros.div_euclid(1_000_000);
        let (year, month, day, hour, minute) = crate::civil::civil_from_unix(secs);
        let path = PathBuf::from(format!(
            "{collector}/bgpdata/{year:04}.{month:02}/{}/ribs.{year:04}{month:02}{day:02}.{hour:02}{minute:02}.gz",
            Stream::Ribs.directory()
        ));
        let timestamp = u32::try_from(secs).unwrap_or(u32::MAX);
        let records = self.rib.encode(timestamp, &self.config.view);
        let mut writer = SegmentWriter::create(&self.config.root, &collector, Stream::Ribs, &path)?;
        for (subtype, payload) in &records {
            let record = frame_records_into(timestamp, *subtype, payload);
            writer.append(&record, boundary_micros)?;
        }
        let state = if reasons.is_empty() {
            "complete"
        } else {
            "partial"
        };
        let sidecar = Sidecar {
            schema_version: 1,
            collector: collector.clone(),
            client: self.config.client.clone(),
            view: self.config.view.clone(),
            feed: "bmp".to_string(),
            stream: Stream::Ribs,
            range_start: render_stamp(boundary_micros),
            range_end: render_stamp(boundary_micros),
            session_epoch: self.config.epoch,
            ordinal: 1,
            path: path.display().to_string(),
            bytes: 0,
            records: 0,
            end_of_rib_records: 0,
            compression: "gzip".to_string(),
            sha256: String::new(),
            state: String::new(),
            partial_reasons: Vec::new(),
            late_arrival: 0,
            exporter_sys_name: self.exporter_name.clone(),
            exporter_sys_descr: self.exporter_descr.clone(),
            first_router_time: None,
            last_router_time: None,
            baseline: self.baseline_rows(),
        };
        let published = writer.finish(sidecar, state, reasons.to_vec(), 0)?;
        if let Err(e) = crate::catalog::append_segment(&self.config.root, &published) {
            eprintln!("bgpviews: catalog append failed: {e}");
        }
        self.ribs.push(PublishedSegment {
            sidecar_path: PathBuf::from(format!("{}.json", path.display())),
            path,
            sidecar: published,
            interval_start: boundary_micros,
        });
        Ok(())
    }

    fn publish(&mut self, current: Open) -> Result<(), CollectorError> {
        let Open {
            writer,
            interval_start,
            ordinal,
            reasons,
            eor,
            first,
            last,
            first_router,
            last_router,
        } = current;
        let path = writer.path().to_path_buf();
        let root = &self.config.root;
        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let state = if reasons.is_empty() {
            "complete"
        } else {
            "partial"
        };
        let sidecar_base = Sidecar {
            schema_version: 1,
            collector: self.config.collector(),
            client: self.config.client.clone(),
            view: self.config.view.clone(),
            feed: "bmp".to_string(),
            stream: Stream::Updates,
            range_start: render_stamp(first),
            range_end: render_stamp(last),
            session_epoch: self.config.epoch,
            ordinal,
            path: relative.display().to_string(),
            bytes: 0,
            records: 0,
            end_of_rib_records: 0,
            compression: "gzip".to_string(),
            sha256: String::new(),
            state: String::new(),
            partial_reasons: Vec::new(),
            late_arrival: 0,
            exporter_sys_name: self.exporter_name.clone(),
            exporter_sys_descr: self.exporter_descr.clone(),
            first_router_time: first_router.map(render_stamp),
            last_router_time: last_router.map(render_stamp),
            baseline: self.baseline_rows(),
        };
        let reasons: Vec<String> = reasons.into_iter().collect();
        if self.config.rib {
            // The boundary is the end of the interval this segment closes.
            let boundary = interval_start + self.config.interval_seconds * 1_000_000;
            self.write_rib(boundary, &reasons)?;
        }
        let sidecar = writer.finish(sidecar_base, state, reasons, eor)?;
        // The catalog is appended only after the sidecar exists: a row that named a
        // segment before its metadata did would be an index entry pointing at nothing.
        if let Err(e) = crate::catalog::append_segment(&self.config.root, &sidecar) {
            eprintln!("bgpviews: catalog append failed: {e}");
        }
        let sidecar_path = PathBuf::from(format!("{}.json", path.display()));
        let _ = interval_start;
        self.published.push(PublishedSegment {
            path,
            sidecar_path,
            sidecar,
            interval_start,
        });
        Ok(())
    }
}

fn stamp(micros: i64) -> Result<Stamp, CollectorError> {
    Stamp::from_parts(
        micros.div_euclid(1_000_000),
        micros.rem_euclid(1_000_000) as u32,
    )
    .map_err(|e| CollectorError::Coverage(e.to_string()))
}

/// Now, in microseconds, from the system clock.
#[must_use]
pub fn now_micros() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_micros() as i64,
        Err(_) => 0,
    }
}

/// Read frames from a stream and feed them to the collector.
///
/// Returns the number of frames fed. A read that ends the stream is not an error:
/// the caller decides whether the session is over.
///
/// # Errors
/// Returns [`CollectorError`] when a frame cannot be read or accepted.
pub fn pump<R: std::io::Read>(
    reader: &mut R,
    collector: &mut Collector,
    clock: &mut dyn FnMut() -> i64,
) -> Result<u64, CollectorError> {
    pump_with_tick(reader, collector, clock, &mut |_: &mut Collector| Ok(()))
}

/// Read frames, calling `tick` when a read times out instead of holding the session.
///
/// A daemon must be able to act between frames: a SIGHUP has to produce a RIB without
/// waiting for the next interval boundary, and a socket with no traffic would otherwise
/// block for as long as the router stays quiet. A read timeout is what turns "waiting
/// for data" into "a chance to check what else is pending".
///
/// # Errors
/// Returns [`CollectorError`] when a frame cannot be read or accepted, or when `tick`
/// fails.
pub fn pump_with_tick<R: std::io::Read>(
    reader: &mut R,
    collector: &mut Collector,
    clock: &mut dyn FnMut() -> i64,
    tick: &mut dyn FnMut(&mut Collector) -> Result<(), CollectorError>,
) -> Result<u64, CollectorError> {
    let mut splitter = StreamSplitter::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut fed = 0u64;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                // A signal that interrupts the read is not a broken session: the bytes
                // are still in the kernel buffer. It is the same opportunity to act as
                // a timeout, and the tick is where a SIGHUP becomes a RIB.
                tick(collector)?;
                continue;
            }
            Err(e) => {
                return Err(CollectorError::Io {
                    op: "read",
                    source: e,
                })
            }
        };
        if read == 0 {
            break;
        }
        splitter.push(&buffer[..read]);
        while let Some(frame) = splitter
            .next_frame()
            .map_err(|e| CollectorError::Frame(crate::bmp::FrameError::Split(e)))?
        {
            collector.on_frame(&frame, clock())?;
            fed += 1;
        }
    }
    Ok(fed)
}

fn interval_name(interval_start: i64, interval_seconds: i64) -> SegmentName {
    let secs = interval_start.div_euclid(1_000_000);
    let aligned = secs.div_euclid(interval_seconds) * interval_seconds;
    let (year, month, day, hour, minute) = crate::civil::civil_from_unix(aligned);
    SegmentName {
        month: format!("{year:04}.{month:02}"),
        file: format!("updates.{year:04}{month:02}{day:02}.{hour:02}{minute:02}"),
    }
}

struct SegmentName {
    month: String,
    file: String,
}

fn segment_path(
    collector: &str,
    interval_start: i64,
    interval_seconds: i64,
    ordinal: u32,
) -> PathBuf {
    let (month, file) = segment_name(interval_start, interval_seconds);
    let suffix = if ordinal > 1 {
        format!(".{ordinal}")
    } else {
        String::new()
    };
    PathBuf::from(format!(
        "{}/bgpdata/{}/{}/{}{}.gz",
        collector,
        month,
        Stream::Updates.directory(),
        file,
        suffix
    ))
}

fn segment_name(micros: i64, interval_seconds: i64) -> (String, String) {
    let secs = micros.div_euclid(1_000_000);
    let aligned = secs.div_euclid(interval_seconds) * interval_seconds;
    let (year, month, day, hour, minute) = crate::civil::civil_from_unix(aligned);
    (
        format!("{year:04}.{month:02}"),
        format!("updates.{year:04}{month:02}{day:02}.{hour:02}{minute:02}"),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::Path;

    fn config(root: &std::path::Path) -> CollectorConfig {
        CollectorConfig {
            client: "edge1".to_string(),
            view: "adj-in-pre".to_string(),
            epoch: 1,
            interval_seconds: 900,
            root: root.to_path_buf(),
            rib: true,
            disk_warning_percent: 90,
            disk_critical_percent: 97,
        }
    }

    fn fixture() -> Vec<u8> {
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lab/fixtures/frr-8.4-two-router.bin"),
        )
        .expect("the lab fixture is committed")
    }

    fn temp(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("bgpviews-collector-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn a_capture_fed_through_a_reader_produces_one_published_segment() {
        let root = temp("pump");
        let mut collector = Collector::open_at(config(&root), 1_794_702_169_000_000).unwrap();
        let capture = fixture();
        let mut cursor = std::io::Cursor::new(capture);
        let mut clock = || 1_794_702_169_000_000i64;
        let fed = pump(&mut cursor, &mut collector, &mut clock).unwrap();
        assert_eq!(fed, 10, "ten frames");
        collector.finish(1_794_702_169_010_000).unwrap();

        assert_eq!(collector.published().len(), 1);
        // Pre-policy only: FRR's two post-policy EoR duplicates are counted, not
        // archived, so six of the eight route records reach the segment.
        assert_eq!(collector.records(), 6);
        assert_eq!(collector.end_of_rib(), 2);
        assert_eq!(collector.unconfigured_view(), 2);
        let segment = &collector.published()[0];
        assert!(segment.path.exists(), "the segment is on disk");
        assert_eq!(segment.sidecar.state, "partial");
        assert!(segment
            .sidecar
            .partial_reasons
            .contains(&"session_ended".to_string()));

        // The coverage log ends with an open gap, and the timeline is contiguous.
        let text = std::fs::read_to_string(collector.coverage_path()).unwrap();
        let (timeline, summary) = Timeline::replay(text.as_bytes());
        assert_eq!(summary.discarded_lines, 0);
        timeline.check_invariants().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn baseline_is_observed_only_from_the_first_marker_of_a_generation() {
        let root = temp("baseline");
        let mut collector = Collector::open_at(config(&root), 1_794_702_169_000_000).unwrap();
        let frames = crate::bmp::frames(&fixture()).unwrap();
        let mut clock = 1_794_702_169_000_000i64;
        for frame in &frames {
            collector.on_frame(&frame.bytes, clock).unwrap();
            clock += 1_000;
        }
        collector.finish(clock).unwrap();

        let segment = &collector.published()[0];
        let observed: Vec<_> = segment
            .sidecar
            .baseline
            .iter()
            .filter(|b| b.baseline == "observed")
            .collect();
        assert!(
            !observed.is_empty(),
            "the capture carries end-of-RIB markers, so at least one family is observed"
        );
        // The capture is pre-policy IPv4 and IPv6 only, so IPv4 unicast is observed
        // and nothing is invented for a family the peer never dumped.
        assert!(observed
            .iter()
            .any(|b| b.afi == 1 && b.safi == 1 && b.generation == 1));
        assert!(observed.iter().all(|b| b.eor_journal_seq.is_some()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_capture_without_any_marker_reports_an_unknown_baseline() {
        let root = temp("baseline-none");
        let mut collector = Collector::open_at(config(&root), 1_794_702_169_000_000).unwrap();
        // Only Peer Up and announcements: no EoR, which is what a collector sees
        // when it was already listening before the peer came up.
        let frames = crate::bmp::frames(&fixture()).unwrap();
        let mut clock = 1_794_702_169_000_000i64;
        for frame in frames
            .iter()
            .filter(|f| !matches!(f.kind, FrameKind::RouteMonitoring { is_eor: true, .. }))
        {
            collector.on_frame(&frame.bytes, clock).unwrap();
            clock += 1_000;
        }
        collector.finish(clock).unwrap();

        let segment = &collector.published()[0];
        assert!(
            segment
                .sidecar
                .baseline
                .iter()
                .all(|b| b.baseline == "unknown"),
            "no marker means no baseline claim: {:?}",
            segment.sidecar.baseline
        );
        // And the segment is still partial only for its own reason, never for the
        // missing marker.
        assert_eq!(
            segment.sidecar.partial_reasons,
            vec!["session_ended".to_string()]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_frame_for_another_view_is_counted_not_archived() {
        let root = temp("view");
        let mut collector = Collector::open_at(config(&root), 1_794_702_169_000_000).unwrap();
        let capture = fixture();
        let frames = crate::bmp::frames(&capture).unwrap();
        // Feed the two post-policy EoR frames only; they belong to another view.
        let mut fed = 0;
        for frame in &frames {
            if matches!(
                frame.kind,
                FrameKind::RouteMonitoring {
                    post_policy: true,
                    ..
                }
            ) {
                collector
                    .on_frame(&frame.bytes, 1_794_702_169_000_000)
                    .unwrap();
                fed += 1;
            }
        }
        assert_eq!(fed, 2, "the capture has two post-policy records");
        assert_eq!(collector.records(), 0, "neither became an MRT record");
        assert_eq!(collector.unconfigured_view(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_short_interval_is_refused_before_any_file_is_created() {
        let root = temp("interval");
        let mut cfg = config(&root);
        cfg.interval_seconds = 30;
        let err = Collector::open(cfg).err().expect("refused");
        assert!(matches!(
            err,
            CollectorError::InvalidInterval { seconds: 30 }
        ));
        assert!(!root.join("sessions").exists(), "nothing was created");
    }
}
