//! Replaying a captured BMP stream into an archive directory.
//!
//! This is the harness the collector will grow into: the same three artifacts, in
//! the same order, driven by a file instead of a socket. It exists so the file
//! formats can be proven before there is a listener, and so the lab's capture can
//! be a test fixture rather than a demonstration.
//!
//! Order matters and is the design's: journal first (raw frames, durable), then
//! the coverage timeline (what covers what), then the segment (derived, published
//! by rename). A reader that finds a segment but no coverage line has found a bug,
//! not a tolerable state.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use crate::bgp4mp::{encode_record, RecordContext};
use crate::bmp::{frames, FrameError, FrameKind};
use crate::coverage::{Line, RangeKind, SegmentRef, Stamp, Timeline};
use crate::journal::{JournalError, JournalWriter};
use crate::segment::{render_stamp, SegmentError, SegmentWriter, Sidecar, Stream};

/// What a replay needs to know that a capture cannot tell it.
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// Configured client name.
    pub client: String,
    /// The view the capture came from.
    pub view: String,
    /// Session epoch to record.
    pub epoch: u64,
    /// Synthetic receive time for the first frame, in microseconds.
    pub receive_start_micros: i64,
    /// How far apart to space frames, in microseconds. A capture carries no
    /// receive clock, so the replay invents one and says so.
    pub frame_spacing_micros: i64,
    /// Interval length in seconds, which decides the segment's name and bounds.
    pub interval_seconds: i64,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            client: "edge1".to_string(),
            view: "adj-in-pre".to_string(),
            epoch: 1,
            // 2026-11-15T00:22:49Z, the second the committed lab capture was taken.
            receive_start_micros: 1794702169 * 1_000_000,
            frame_spacing_micros: 1_000,
            interval_seconds: 900,
        }
    }
}

impl ReplayOptions {
    /// The collector name, `<client>-<view>`.
    #[must_use]
    pub fn collector(&self) -> String {
        format!("{}-{}", self.client, self.view)
    }
}

/// Why a replay failed.
#[derive(Debug)]
pub enum ReplayError {
    /// The capture could not be split or classified.
    Frame(FrameError),
    /// The journal could not be written.
    Journal(JournalError),
    /// The segment could not be written.
    Segment(SegmentError),
    /// The coverage timeline could not be written or violated an invariant.
    Coverage(String),
    /// A file could not be written.
    Io {
        /// What was being attempted.
        op: &'static str,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A route record arrived for a peer with no Peer Up, so the envelope has no
    /// local AS or local address to carry.
    MissingPeerUp {
        /// The peer address.
        peer: IpAddr,
    },
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame(e) => write!(f, "{e}"),
            Self::Journal(e) => write!(f, "{e}"),
            Self::Segment(e) => write!(f, "{e}"),
            Self::Coverage(detail) => write!(f, "coverage: {detail}"),
            Self::Io { op, source } => write!(f, "{op} failed: {source}"),
            Self::MissingPeerUp { peer } => {
                write!(f, "route record for peer {peer} arrived before its Peer Up")
            }
        }
    }
}

impl std::error::Error for ReplayError {}

/// What a replay produced, as paths and counts rather than prose.
#[derive(Debug, Clone)]
pub struct ReplayReport {
    /// The collector name.
    pub collector: String,
    /// The session journal part.
    pub journal: PathBuf,
    /// The published MRT segment.
    pub segment: PathBuf,
    /// Its sidecar.
    pub sidecar: PathBuf,
    /// The coverage log.
    pub coverage: PathBuf,
    /// Frames journaled.
    pub frames: u64,
    /// MRT records written.
    pub records: u64,
    /// Of those, end-of-RIB markers.
    pub end_of_rib: u64,
    /// The range the segment covers, in microseconds.
    pub range: (i64, i64),
    /// The sidecar as written, for a caller that wants the hash without a re-read.
    pub sidecar_fields: Sidecar,
}

/// Replay `capture` into `root`, which is created if it does not exist.
///
/// # Errors
/// Returns [`ReplayError`] when the capture cannot be split, a record names a peer
/// that never came up, or an artifact cannot be written.
pub fn replay_capture(
    root: &Path,
    capture: &[u8],
    options: &ReplayOptions,
) -> Result<ReplayReport, ReplayError> {
    let collector = options.collector();
    let parsed = frames(capture).map_err(ReplayError::Frame)?;

    // 1. The journal: every frame, exactly as received.
    let journal_path = PathBuf::from(format!(
        "sessions/{}/{:016}/journal.0.bin",
        options.client, options.epoch
    ));
    let mut journal =
        JournalWriter::create(&root.join(&journal_path)).map_err(ReplayError::Journal)?;

    let mut receive_at: Vec<i64> = Vec::with_capacity(parsed.len());
    for (index, frame) in parsed.iter().enumerate() {
        let micros = options.receive_start_micros + index as i64 * options.frame_spacing_micros;
        receive_at.push(micros);
        journal
            .append(
                micros.div_euclid(1_000_000),
                micros.rem_euclid(1_000_000) as u32,
                &frame.bytes,
            )
            .map_err(ReplayError::Journal)?;
    }
    let journal_frames = journal.frames();
    drop(journal);

    // 2. Peer Up state, which is where local AS and local address come from.
    let mut peers: HashMap<IpAddr, (u32, u32, IpAddr)> = HashMap::new();
    let mut exporter_name = None;
    let mut exporter_descr = None;
    for frame in &parsed {
        match &frame.kind {
            FrameKind::Initiation {
                sys_name,
                sys_descr,
            } => {
                exporter_name = sys_name.clone();
                exporter_descr = sys_descr.clone();
            }
            FrameKind::PeerUp {
                peer_ip,
                peer_asn,
                local_ip,
                local_asn,
                ..
            } => {
                peers.insert(*peer_ip, (*peer_asn, *local_asn, *local_ip));
            }
            _ => {}
        }
    }

    // 3. Coverage and the segment, in one pass over the route records.
    let first_route = parsed
        .iter()
        .position(|f| matches!(f.kind, FrameKind::RouteMonitoring { .. }))
        .ok_or_else(|| ReplayError::Coverage("the capture carries no route records".to_string()))?;
    let last_receive = receive_at
        .last()
        .copied()
        .unwrap_or(options.receive_start_micros);
    let first_receive = receive_at[first_route];

    let interval = options.interval_seconds * 1_000_000;
    let interval_start = first_receive.div_euclid(interval) * interval;
    let name = segment_name(first_receive, options.interval_seconds);
    // The design's layout: bgpdata/YYYY.MM/UPDATES/updates.YYYYMMDD.HHMM.gz
    let relative = PathBuf::from(format!(
        "{}/bgpdata/{}/{}/{}.gz",
        collector,
        name.month,
        Stream::Updates.directory(),
        name.file
    ));

    let sidecar_base = Sidecar {
        schema_version: 1,
        collector: collector.clone(),
        client: options.client.clone(),
        view: options.view.clone(),
        feed: "bmp".to_string(),
        stream: Stream::Updates,
        range_start: render_stamp(first_receive),
        range_end: render_stamp(last_receive),
        session_epoch: options.epoch,
        ordinal: 1,
        path: relative.display().to_string(),
        bytes: 0,
        records: 0,
        end_of_rib_records: 0,
        compression: "gzip".to_string(),
        sha256: String::new(),
        state: String::new(),
        partial_reasons: Vec::new(),
        late_arrival: 0,
        exporter_sys_name: exporter_name.clone(),
        exporter_sys_descr: exporter_descr.clone(),
        first_router_time: None,
        last_router_time: None,
    };

    let mut first_router: Option<i64> = None;
    let mut last_router: Option<i64> = None;
    let mut segment = SegmentWriter::create(root, &collector, Stream::Updates, &relative)
        .map_err(ReplayError::Segment)?;
    let mut end_of_rib = 0u64;
    for (index, frame) in parsed.iter().enumerate() {
        let FrameKind::RouteMonitoring {
            peer_ip,
            asn_len,
            is_eor,
            router_time,
            pdu_offset,
            ..
        } = &frame.kind
        else {
            continue;
        };
        let Some((peer_asn, local_asn, local_ip)) = peers.get(peer_ip).copied() else {
            return Err(ReplayError::MissingPeerUp { peer: *peer_ip });
        };
        let when = receive_at[index];
        let pdu = &frame.bytes[*pdu_offset..];
        let context = RecordContext {
            seconds: when.div_euclid(1_000_000) as u32,
            micros: when.rem_euclid(1_000_000) as u32,
            peer_asn,
            local_asn,
            peer_ip: *peer_ip,
            local_ip,
            interface_index: 0,
            asn_len: *asn_len,
        };
        let record =
            encode_record(&context, pdu).map_err(|e| ReplayError::Coverage(e.to_string()))?;
        segment
            .append(&record, when)
            .map_err(ReplayError::Segment)?;
        if *is_eor {
            end_of_rib += 1;
        }
        if let Some(router) = router_time {
            let router_micros = (router * 1_000_000.0) as i64;
            first_router.get_or_insert(router_micros);
            last_router = Some(router_micros);
        }
    }

    // The capture ends, so the session ends: the segment is partial for that
    // reason, not silently complete.
    let mut reasons = vec!["session_ended".to_string()];
    reasons.sort();
    let mut sidecar_base = sidecar_base;
    sidecar_base.first_router_time = first_router.map(render_stamp);
    sidecar_base.last_router_time = last_router.map(render_stamp);
    let sidecar = segment
        .finish(sidecar_base, "partial", reasons, end_of_rib)
        .map_err(ReplayError::Segment)?;

    // 4. The coverage timeline for the range the segment covers.
    let mut timeline = Timeline::default();
    let mut lines = Vec::new();
    let mut push = |line: Line, timeline: &mut Timeline| -> Result<(), ReplayError> {
        timeline
            .apply(&line)
            .map_err(|e| ReplayError::Coverage(e.to_string()))?;
        lines.push(crate::coverage::encode_line(&line));
        Ok(())
    };
    // A timeline starts as a gap: before the first record there is nothing to
    // cover, and this is the only way a segment range can name its artifact.
    push(
        Line::Open {
            at: Stamp::from_parts(
                options.receive_start_micros.div_euclid(1_000_000),
                options.receive_start_micros.rem_euclid(1_000_000) as u32,
            )
            .map_err(|e| ReplayError::Coverage(e.to_string()))?,
            range_kind: RangeKind::Gap,
            reason: Some("no_session".to_string()),
        },
        &mut timeline,
    )?;
    push(
        Line::Transition {
            at: Stamp::from_parts(
                first_receive.div_euclid(1_000_000),
                first_receive.rem_euclid(1_000_000) as u32,
            )
            .map_err(|e| ReplayError::Coverage(e.to_string()))?,
            range_kind: RangeKind::Segment,
            reason: None,
            segment: Some(SegmentRef {
                session_epoch: options.epoch,
                ordinal: 1,
            }),
            carried_reasons: Vec::new(),
            carried_through_seq: None,
        },
        &mut timeline,
    )?;
    push(
        Line::Transition {
            at: Stamp::from_parts(
                (last_receive + 1).div_euclid(1_000_000),
                (last_receive + 1).rem_euclid(1_000_000) as u32,
            )
            .map_err(|e| ReplayError::Coverage(e.to_string()))?,
            range_kind: RangeKind::Gap,
            reason: Some("session_ended".to_string()),
            segment: None,
            carried_reasons: Vec::new(),
            carried_through_seq: None,
        },
        &mut timeline,
    )?;
    timeline.check_invariants().map_err(ReplayError::Coverage)?;

    let coverage_path = PathBuf::from(format!("coverage/{collector}/updates.jsonl"));
    let full_coverage = root.join(&coverage_path);
    if let Some(parent) = full_coverage.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ReplayError::Io {
            op: "create coverage directory",
            source: e,
        })?;
    }
    std::fs::write(&full_coverage, lines.concat()).map_err(|e| ReplayError::Io {
        op: "write coverage log",
        source: e,
    })?;

    let _ = interval_start;
    Ok(ReplayReport {
        collector,
        journal: root.join(journal_path),
        segment: root.join(&relative),
        sidecar: root.join(format!("{}.json", relative.display())),
        coverage: full_coverage,
        frames: journal_frames,
        records: sidecar.records,
        end_of_rib,
        range: (first_receive, last_receive),
        sidecar_fields: sidecar,
    })
}

struct SegmentName {
    month: String,
    file: String,
}

fn segment_name(micros: i64, interval_seconds: i64) -> SegmentName {
    let secs = micros.div_euclid(1_000_000);
    let aligned = secs.div_euclid(interval_seconds) * interval_seconds;
    let (year, month, day, hour, minute) = civil_from_unix(aligned);
    SegmentName {
        month: format!("{year:04}.{month:02}"),
        file: format!("updates.{year:04}{month:02}{day:02}.{hour:02}{minute:02}"),
    }
}

/// Days-from-civil in reverse, without a date library: the archive needs a UTC
/// name, not a calendar.
fn civil_from_unix(seconds: i64) -> (i64, u32, u32, u32, u32) {
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let hour = (secs_of_day / 3600) as u32;
    let minute = ((secs_of_day % 3600) / 60) as u32;

    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, minute)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn segment_names_follow_the_interval_start_in_utc() {
        // 2026-11-15T00:22:49Z, the second the committed lab capture was taken.
        let micros = 1794702169i64 * 1_000_000;
        let name = segment_name(micros, 900);
        assert_eq!(name.month, "2026.11");
        assert_eq!(name.file, "updates.20261115.0015");
    }

    #[test]
    fn civil_dates_match_known_epochs() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0));
        assert_eq!(civil_from_unix(1_000_000_000), (2001, 9, 9, 1, 46));
        assert_eq!(civil_from_unix(1794702169), (2026, 11, 15, 0, 22));
    }
}
