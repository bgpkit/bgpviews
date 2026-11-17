//! Republishing a segment the coverage log claims and no artifact satisfies.
//!
//! Design reference: `docs/design.md` sections 5.5 and 11. A writer killed between the
//! coverage transition that opens a segment and the rename that publishes it leaves two
//! true statements behind: the timeline says records were placed in that range, and no
//! MRT file exists for it. Recovery closes the range at the last frame the journal proves
//! and deletes the temporary file, which leaves the claim with nothing to point at, so
//! `verify` reports it `missing` until something rebuilds it. Section 11 says such a
//! segment is "rebuilt as partial with `writer_restart`".
//!
//! The rebuild is a replay through the live collector, not a second encoder. The
//! collector is the only definition of what a record is, so [`republish`] builds one in a
//! throwaway archive root under the system temporary directory, feeds it the frames of the
//! dead session's journal up to the claim's end, and takes the segment it produced. The
//! scratch root is what keeps the rebuild off the real archive: opening a collector on the
//! real root would append coverage lines for a session that never existed, allocate a fresh
//! epoch, and hand out an ordinal the claim already owns. It is removed when the rebuild
//! returns, on the success and the failure path alike.
//!
//! Two rules are worth stating because they are easy to get wrong:
//!
//! - **Receive times come from the journal.** The rebuild never reads the wall clock, so
//!   every timestamp in the rebuilt file is one the dead writer could have written.
//! - **Nothing is invented.** A frame the live path would refuse is still refused: the
//!   rebuild stops at the last good record, marks the segment partial with the reason the
//!   refusal earns, and publishes what the journal proved. A range the journal cannot
//!   prove at all is reported, not guessed.
//!
//! The rebuild feeds one session's journal from its beginning, because a Route Monitoring
//! frame is only encodable when the Peer Up that gives its envelope the local AS and
//! address has been seen. That holds the frames in memory once, at startup, before a
//! socket is accepted.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::collector::{Collector, CollectorConfig, CollectorError};
use crate::journal;
use crate::recovery::UnpublishedClaim;
use crate::segment::{render_stamp, sha256_hex, SegmentError, SegmentWriter, Sidecar, Stream};

/// What republishing one claim produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The claim is now satisfied by an artifact this process wrote.
    Republished(Artifact),
    /// The claim was already satisfied by a published pair; nothing was written.
    AlreadyPublished(PathBuf),
    /// The claim cannot be rebuilt from what is on disk, and nothing was written.
    Unresolved {
        /// Why, in the form an operator can act on.
        reason: String,
    },
}

/// A segment this module rebuilt and published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// The published MRT path.
    pub path: PathBuf,
    /// The sidecar written beside it.
    pub sidecar_path: PathBuf,
    /// MRT records inside it.
    pub records: u64,
    /// Bytes of the compressed file.
    pub bytes: u64,
    /// The reasons it is partial for, sorted. Always holds `writer_restart`.
    pub partial_reasons: Vec<String>,
}

/// Why a rebuild failed.
///
/// These are failures of *this* process to write its own files, not findings about the
/// archive: a claim that cannot be rebuilt from what is on disk is reported as
/// [`Outcome::Unresolved`] instead, because it is a fact about the journal rather than an
/// error in the repair.
#[derive(Debug)]
pub enum RepublishError {
    /// A file could not be read or written.
    Io {
        /// What was being attempted.
        op: &'static str,
        /// The path it was attempted on.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The rebuilt segment could not be written to the scratch root.
    Segment(SegmentError),
    /// The replay through the collector failed for a reason that is not a refusal of one
    /// frame: the rebuild has no honest artifact to publish.
    Replay(String),
}

impl std::fmt::Display for RepublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { op, path, source } => {
                write!(f, "{op} failed for {}: {source}", path.display())
            }
            Self::Segment(e) => write!(f, "rebuilt segment could not be written: {e}"),
            Self::Replay(detail) => write!(f, "rebuild replay failed: {detail}"),
        }
    }
}

impl std::error::Error for RepublishError {}

impl From<SegmentError> for RepublishError {
    fn from(value: SegmentError) -> Self {
        Self::Segment(value)
    }
}

/// Rebuild the segment `claim` names from the journal of the epoch it names.
///
/// `config` is the collector configuration this client runs with. Its `root` is the real
/// archive; `epoch` is ignored, because the claim names the epoch being rebuilt rather
/// than the one about to start, and `rib` is forced off: a RIB is derived evidence and
/// never part of this repair.
///
/// The identity is the claim's, never a fresh one: the artifact lands at the path that
/// `interval_start` and `ordinal` imply, its sidecar carries the claim's
/// `range_start`/`session_epoch`/`ordinal`, and `range_end` is the claim's `until`. That
/// is what makes the coverage claim resolve to an artifact instead of being rewritten.
///
/// # Errors
/// Returns [`RepublishError`] only when this process cannot read the journal or write its
/// own files. A claim the archive cannot prove is [`Outcome::Unresolved`].
pub fn republish(
    config: &CollectorConfig,
    claim: &UnpublishedClaim,
) -> Result<Outcome, RepublishError> {
    if config.feed != "bmp" {
        // A BGP feed journals the UPDATE without the peer identity the envelope needs, so
        // there is no honest way to turn those frames back into BGP4MP records.
        return Ok(Outcome::Unresolved {
            reason: format!(
                "a {} feed journals messages without the peer context a rebuild needs",
                config.feed
            ),
        });
    }
    if config.interval_seconds < 60 || config.interval_seconds % 60 != 0 {
        return Ok(Outcome::Unresolved {
            reason: format!(
                "interval {}s is not a whole number of minutes, so no segment name can be derived",
                config.interval_seconds
            ),
        });
    }

    let collector = config.collector();
    // A rebuilt segment is partial by definition, so it is published under the partial name
    // the collector would have used for the same identity.
    let relative = crate::segment::published_name(
        &segment_relative_path(
            &collector,
            claim.interval_start,
            config.interval_seconds,
            claim.ordinal,
        ),
        "partial",
        claim.ordinal,
    );
    let target = config.root.join(&relative);
    let sidecar_path = sidecar_path(&target);
    let range_start = render_stamp(claim.interval_start);
    let range_end = render_stamp(claim.until);

    // A published pair is the claim satisfied. Never replace it: a published file is
    // immutable, and a rebuild that overwrote one would destroy the record it was meant to
    // repair.
    if sidecar_path.is_file() {
        if !target.is_file() {
            return Ok(Outcome::Unresolved {
                reason: format!(
                    "{} exists but its artifact does not; verify reports this as missing and \
                     only an operator can decide what it should be",
                    sidecar_path.display()
                ),
            });
        }
        return match published_identity(&sidecar_path)? {
            Some((start, epoch, ordinal))
                if start == range_start
                    && epoch == claim.session_epoch
                    && ordinal == claim.ordinal =>
            {
                Ok(Outcome::AlreadyPublished(target))
            }
            Some(_) => Ok(Outcome::Unresolved {
                reason: format!(
                    "{} names a different segment; refusing to replace it",
                    sidecar_path.display()
                ),
            }),
            None => Ok(Outcome::Unresolved {
                reason: format!(
                    "{} is not a schema 1 sidecar; refusing to replace it",
                    sidecar_path.display()
                ),
            }),
        };
    }
    if target.exists() {
        // An artifact with no sidecar is not a published pair: publication is the MRT and the
        // sidecar together, and a reader checks the second before trusting the first. This is
        // either an earlier attempt of ours that died between the two renames, or the dead
        // writer's own crash in the same window. Either way the rebuild produces the records
        // the claim covers, which is at least what the stranded file holds, so replacing it
        // loses nothing a reader could have verified.
        std::fs::remove_file(&target).map_err(|e| RepublishError::Io {
            op: "remove an unpublished artifact",
            path: target.clone(),
            source: e,
        })?;
    }

    let frames = match epoch_frames(&config.root, &config.client, claim.session_epoch) {
        Ok(frames) => frames,
        Err(reason) => return Ok(Outcome::Unresolved { reason }),
    };
    let Some(last_at) = frames.iter().map(|frame| frame.at).max() else {
        return Ok(Outcome::Unresolved {
            reason: format!(
                "the journal of epoch {} holds no complete frame",
                claim.session_epoch
            ),
        });
    };
    if last_at.saturating_add(1) < claim.until {
        return Ok(Outcome::Unresolved {
            reason: format!(
                "the journal's last frame is at {}, before the claim ends at {range_end}: the \
                 part that would prove the range is not retained",
                render_stamp(last_at)
            ),
        });
    }

    // Everything the rebuild writes goes into a scratch root outside the archive. It is
    // removed when this function returns, on every path, by the guard.
    let scratch = Scratch::create()?;
    let scratch_config = CollectorConfig {
        client: config.client.clone(),
        view: config.view.clone(),
        feed: "bmp".to_string(),
        // The claim's epoch, so the sidecar the collector builds names the session the
        // records actually belonged to even before it is overridden below.
        epoch: claim.session_epoch,
        interval_seconds: config.interval_seconds,
        root: scratch.path().to_path_buf(),
        // A RIB is derived from the update stream and is not this repair's business.
        rib: false,
        disk_warning_percent: config.disk_warning_percent,
        disk_critical_percent: config.disk_critical_percent,
        write_stall_ms: config.write_stall_ms,
        backpressure_deadline_seconds: config.backpressure_deadline_seconds,
        journal_max_bytes: 0,
        journal_rotate_seconds: 0,
        journal_sync_bytes: 0,
    };
    let opened_at = frames.first().map_or(claim.interval_start, |frame| {
        frame.at.min(claim.interval_start)
    });
    let mut builder = Collector::open_at(scratch_config, opened_at)
        .map_err(|e| RepublishError::Replay(e.to_string()))?;

    // Feed from the beginning of the epoch, and only up to the claim's end: a frame that
    // belongs after the claimed range must not extend it.
    let mut refused = None;
    for frame in &frames {
        if frame.at >= claim.until {
            continue;
        }
        if let Err(e) = builder.on_frame(&frame.payload, frame.at) {
            match refusal(&e) {
                Some(reason) => {
                    refused = Some(reason);
                    break;
                }
                None => return Err(RepublishError::Replay(e.to_string())),
            }
        }
    }
    // Publish whatever the journal proved. Publishing an empty segment here is correct:
    // the coverage log claims the range, and an empty MRT file is how a range with no
    // records is written (design section 5.4), not an absence of one.
    builder
        .finish(claim.until)
        .map_err(|e| RepublishError::Replay(e.to_string()))?;

    let aligned = aligned_start(claim.interval_start, config.interval_seconds);
    // The claimed segment is the one that was *open* when the writer died, so it is the last
    // segment the journal order produces for that interval. Taking the first match would pick
    // the wrong one when a frame whose receive time steps back opened a second segment for the
    // same interval later in the journal, which the live path does too.
    let produced = builder
        .published()
        .iter()
        .rev()
        .find(|segment| segment.interval_start == aligned)
        .cloned();
    let (source, produced_sidecar) = match produced {
        Some(segment) => (segment.path, segment.sidecar),
        // The claimed range holds no frame at all: the live writer would have closed an
        // empty file there, and the claim says it did.
        None => empty_segment(&scratch, &collector, config, claim, refused)?,
    };

    let bytes = std::fs::read(&source).map_err(|e| RepublishError::Io {
        op: "read the rebuilt segment",
        path: source.clone(),
        source: e,
    })?;
    // The MRT goes first, unlike the live path, which renames the sidecar first (design
    // section 7). No transaction record makes either order authoritative here: a crash
    // between the two renames has to leave something the next start can simply rebuild, and
    // an artifact without a sidecar is exactly that, while a sidecar without its artifact
    // would have to be left for an operator to resolve.
    write_atomically(&target, &bytes, "publish the rebuilt segment")?;

    let mut sidecar = produced_sidecar;
    sidecar.collector = collector;
    sidecar.client = config.client.clone();
    sidecar.view = config.view.clone();
    sidecar.feed = config.feed.clone();
    sidecar.stream = Stream::Updates;
    sidecar.range_start = range_start;
    sidecar.range_end = range_end;
    sidecar.session_epoch = claim.session_epoch;
    sidecar.ordinal = claim.ordinal;
    sidecar.path = relative;
    sidecar.bytes = bytes.len() as u64;
    sidecar.sha256 = sha256_hex(&bytes);
    sidecar.state = "partial".to_string();
    sidecar.partial_reasons = partial_reasons(&sidecar, refused);
    let json = serde_json::to_string_pretty(&sidecar)
        .map_err(|e| RepublishError::Replay(format!("the sidecar cannot be serialized: {e}")))?;
    write_atomically(
        &sidecar_path,
        format!("{json}\n").as_bytes(),
        "publish the rebuilt sidecar",
    )?;

    // The catalog is a projection, so a failure here is reported and costs nothing: a later
    // `rebuild-index` derives the same row from the sidecar that is now on disk.
    if let Err(e) = crate::catalog::append_segment(&config.root, &sidecar) {
        eprintln!("bgpviews: catalog append failed for a rebuilt segment: {e}");
    }

    Ok(Outcome::Republished(Artifact {
        path: target,
        sidecar_path,
        records: sidecar.records,
        bytes: sidecar.bytes,
        partial_reasons: sidecar.partial_reasons.clone(),
    }))
}

/// One journaled frame, with the receive time the journal recorded.
struct Framed {
    at: i64,
    payload: Vec<u8>,
}

/// Every frame of one session epoch, in journal order.
///
/// A torn tail is not an error: the bytes after the last complete frame were never
/// acknowledged, and recovery keeps exactly that prefix.
fn epoch_frames(root: &Path, client: &str, epoch: u64) -> Result<Vec<Framed>, String> {
    let dir = root
        .join("sessions")
        .join(client)
        .join(format!("{epoch:016}"));
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Err(format!(
            "no journal for epoch {epoch} of client {client}: {} is not there",
            dir.display()
        ));
    };
    let mut parts: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("journal.") else {
            continue;
        };
        let Some(number) = rest.strip_suffix(".bin") else {
            continue;
        };
        let Ok(number) = number.parse::<u64>() else {
            continue;
        };
        parts.push((number, entry.path()));
    }
    if parts.is_empty() {
        return Err(format!(
            "no journal part for epoch {epoch} of client {client}: {} holds none",
            dir.display()
        ));
    }
    // Numeric order, because `journal.10.bin` sorts before `journal.2.bin` as text.
    parts.sort();
    let mut frames = Vec::new();
    for (_, part) in parts {
        let (found, _summary) =
            journal::replay(&part).map_err(|e| format!("{}: {e}", part.display()))?;
        for frame in found {
            let at = frame
                .seconds
                .checked_mul(1_000_000)
                .and_then(|scaled| scaled.checked_add(i64::from(frame.micros)))
                .ok_or_else(|| {
                    format!(
                        "{}: frame {} carries a receive time that cannot be represented",
                        part.display(),
                        frame.seq
                    )
                })?;
            frames.push(Framed {
                at,
                payload: frame.payload,
            });
        }
    }
    Ok(frames)
}

/// The reason a refused frame earns, or `None` when the failure is not about the frame.
///
/// The closed set in design section 9 has no `missing_peer_up`; a Route Monitoring frame
/// whose Peer Up is not in the journal means the range is not the contiguous stream the
/// dead session had, which is exactly `journal_corrupt`.
fn refusal(error: &CollectorError) -> Option<&'static str> {
    match error {
        CollectorError::Frame(_) => Some("parse_error"),
        CollectorError::MissingPeerUp { .. } => Some("journal_corrupt"),
        CollectorError::Backpressure { .. } => Some("backpressure"),
        _ => None,
    }
}

/// The reasons the rebuilt segment is partial for, sorted.
///
/// `session_ended` is dropped: it is what the synthetic close inside the scratch collector
/// adds, not a fact the dead session's journal contains. Everything else the collector
/// derived from the frames is kept, and `writer_restart` is always present because the
/// segment is a restart's reconstruction whatever else happened.
fn partial_reasons(sidecar: &Sidecar, refused: Option<&'static str>) -> Vec<String> {
    let mut reasons: BTreeSet<String> = sidecar
        .partial_reasons
        .iter()
        .filter(|reason| reason.as_str() != "session_ended")
        .cloned()
        .collect();
    reasons.insert("writer_restart".to_string());
    if let Some(reason) = refused {
        reasons.insert(reason.to_string());
    }
    reasons.into_iter().collect()
}

/// Write a file by rename, the way every other artifact in this archive is published.
///
/// The temporary file keeps the archive's own `*.tmp` convention, so a crash here leaves
/// something recovery already knows how to delete.
fn write_atomically(path: &Path, bytes: &[u8], op: &'static str) -> Result<(), RepublishError> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RepublishError::Io {
            op: "create the segment directory",
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    let mut temp = path.as_os_str().to_os_string();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut file = std::fs::File::create(&temp).map_err(|e| RepublishError::Io {
        op,
        path: temp.clone(),
        source: e,
    })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| RepublishError::Io {
            op,
            path: temp.clone(),
            source: e,
        })?;
    drop(file);
    std::fs::rename(&temp, path).map_err(|e| RepublishError::Io {
        op,
        path: path.to_path_buf(),
        source: e,
    })?;
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// The sidecar path that belongs to an artifact path.
fn sidecar_path(artifact: &Path) -> PathBuf {
    PathBuf::from(format!("{}.json", artifact.display()))
}

/// What a published sidecar says its identity is, or `None` when it is not a sidecar.
fn published_identity(path: &Path) -> Result<Option<(String, u64, u32)>, RepublishError> {
    let text = std::fs::read_to_string(path).map_err(|e| RepublishError::Io {
        op: "read an existing sidecar",
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(serde_json::from_str::<Sidecar>(&text)
        .ok()
        .map(|sidecar| (sidecar.range_start, sidecar.session_epoch, sidecar.ordinal)))
}

/// The interval boundary a receive time falls in, in microseconds.
fn aligned_start(interval_start: i64, interval_seconds: i64) -> i64 {
    let span = interval_seconds.saturating_mul(1_000_000);
    interval_start.div_euclid(span) * span
}

/// The relative path the design's naming rules give a segment.
///
/// This mirrors `Collector`'s own naming, which is private because a segment's name is the
/// collector's business; a rebuild has to reproduce it exactly, and the unit tests assert
/// the two agree rather than trusting the copy.
fn segment_relative_path(
    collector: &str,
    interval_start: i64,
    interval_seconds: i64,
    ordinal: u32,
) -> String {
    let secs = interval_start.div_euclid(1_000_000);
    let aligned = secs.div_euclid(interval_seconds) * interval_seconds;
    let (year, month, day, hour, minute) = crate::civil::civil_from_unix(aligned);
    let suffix = if ordinal > 1 {
        format!(".{ordinal}")
    } else {
        String::new()
    };
    format!(
        "{collector}/bgpdata/{year:04}.{month:02}/{}/updates.{year:04}{month:02}{day:02}.{hour:02}{minute:02}{suffix}.gz",
        Stream::Updates.directory()
    )
}

/// An empty segment for a claimed range no frame falls in, written by the same writer the
/// collector uses so the file is the same kind of artifact.
fn empty_segment(
    scratch: &Scratch,
    collector: &str,
    config: &CollectorConfig,
    claim: &UnpublishedClaim,
    refused: Option<&'static str>,
) -> Result<(PathBuf, Sidecar), RepublishError> {
    let relative = crate::segment::published_name(
        &segment_relative_path(
            collector,
            claim.interval_start,
            config.interval_seconds,
            claim.ordinal,
        ),
        "partial",
        claim.ordinal,
    );
    let writer = SegmentWriter::create(
        scratch.path(),
        collector,
        Stream::Updates,
        Path::new(&relative),
    )?;
    let base = Sidecar {
        schema_version: 1,
        collector: collector.to_string(),
        client: config.client.clone(),
        view: config.view.clone(),
        feed: config.feed.clone(),
        stream: Stream::Updates,
        range_start: render_stamp(claim.interval_start),
        range_end: render_stamp(claim.until),
        session_epoch: claim.session_epoch,
        ordinal: claim.ordinal,
        path: relative.clone(),
        bytes: 0,
        records: 0,
        end_of_rib_records: 0,
        compression: "gzip".to_string(),
        sha256: String::new(),
        state: String::new(),
        partial_reasons: Vec::new(),
        late_arrival: 0,
        exporter_sys_name: None,
        exporter_sys_descr: None,
        first_router_time: None,
        last_router_time: None,
        baseline: Vec::new(),
    };
    let reasons = partial_reasons(&base, refused);
    let sidecar = writer.finish(base, "partial", reasons, 0)?;
    Ok((scratch.path().join(&relative), sidecar))
}

/// A throwaway archive root, removed when it goes out of scope.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn create() -> Result<Self, RepublishError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "bgpviews-republish-{}-{unique}-{nanos}",
            std::process::id()
        ));
        // A leftover from a process whose id was reused would make `Collector::open_at`
        // replay a stale coverage log; the name is ours, so clearing it is safe.
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).map_err(|e| RepublishError::Io {
            op: "create the scratch archive",
            path: root.clone(),
            source: e,
        })?;
        Ok(Self { root })
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::bmp::FrameError;

    fn fixture_frames() -> Vec<Vec<u8>> {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lab/fixtures/frr-8.4-two-router.bin");
        let bytes = std::fs::read(path).expect("the lab fixture is committed");
        crate::bmp::frames(&bytes)
            .expect("the fixture splits")
            .into_iter()
            .map(|frame| frame.bytes)
            .collect()
    }

    fn config(root: &Path) -> CollectorConfig {
        CollectorConfig {
            client: "edge1".to_string(),
            view: "adj-in-pre".to_string(),
            feed: "bmp".to_string(),
            epoch: 1,
            interval_seconds: 900,
            root: root.to_path_buf(),
            rib: false,
            disk_warning_percent: 90,
            disk_critical_percent: 97,
            write_stall_ms: 2_000,
            backpressure_deadline_seconds: 300,
            journal_max_bytes: 0,
            journal_rotate_seconds: 0,
            journal_sync_bytes: 0,
        }
    }

    fn temp(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "bgpviews-republish-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// Reproduce what a SIGKILL leaves: a journal with every frame, a coverage range that
    /// claims a segment, no published artifact, and a temporary file recovery will remove.
    fn dead_writer(root: &Path, frames: &[Vec<u8>], start: i64, spacing: i64) -> UnpublishedClaim {
        let mut writer = Collector::open_at(config(root), start).unwrap();
        for (index, frame) in frames.iter().enumerate() {
            writer
                .on_frame(frame, start + index as i64 * spacing)
                .unwrap();
        }
        // No finish: the process died with the segment open and never renamed it.
        drop(writer);
        let report = crate::recovery::recover(root, "edge1-adj-in-pre", "edge1").unwrap();
        assert_eq!(report.claims.len(), 1, "{:?}", report.claims);
        report.claims[0].clone()
    }

    fn read_sidecar(path: &Path) -> Sidecar {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn a_claim_with_a_valid_journal_is_republished_under_its_own_identity() {
        let root = temp("identity");
        let frames = fixture_frames();
        let start = 1_794_702_169_000_000;
        let claim = dead_writer(&root, &frames, start, 1_000);
        // The segment opened at the first Route Monitoring frame, not at the Initiation.
        assert_eq!(claim.interval_start, start + 2_000);
        assert_eq!(
            claim.until,
            start + 9_001,
            "one microsecond past the last frame"
        );
        assert_eq!(claim.session_epoch, 1);
        assert_eq!(claim.ordinal, 1);

        let artifact = match republish(&config(&root), &claim).unwrap() {
            Outcome::Republished(artifact) => artifact,
            other => panic!("expected a rebuild, got {other:?}"),
        };

        // 1. The artifact is at exactly the path the claim's interval and ordinal imply, under
        // the partial name every rebuilt segment carries.
        let relative = crate::segment::published_name(
            &segment_relative_path(
                "edge1-adj-in-pre",
                claim.interval_start,
                config(&root).interval_seconds,
                claim.ordinal,
            ),
            "partial",
            claim.ordinal,
        );
        assert_eq!(
            relative, "edge1-adj-in-pre/bgpdata/2026.11/UPDATES/updates.20261115.0015.partial.gz",
            "the name comes from the aligned interval minute and the state"
        );
        assert_eq!(artifact.path, root.join(&relative));
        assert!(artifact.path.is_file(), "the rebuilt artifact exists");

        // 2. The sidecar speaks for the claim, not for a fresh segment.
        let sidecar = read_sidecar(&artifact.sidecar_path);
        assert_eq!(sidecar.collector, "edge1-adj-in-pre");
        assert_eq!(sidecar.range_start, render_stamp(claim.interval_start));
        assert_eq!(sidecar.range_end, render_stamp(claim.until));
        assert_eq!(sidecar.session_epoch, claim.session_epoch);
        assert_eq!(sidecar.ordinal, claim.ordinal);
        assert_eq!(sidecar.state, "partial");
        assert_eq!(sidecar.partial_reasons, vec!["writer_restart".to_string()]);

        // 3. The hash, size and count describe the bytes on disk.
        let bytes = std::fs::read(&artifact.path).unwrap();
        assert_eq!(sidecar.bytes, bytes.len() as u64);
        assert_eq!(sidecar.sha256, sha256_hex(&bytes));
        assert_eq!(sidecar.records, 6, "the six pre-policy records");
        assert_eq!(artifact.records, 6);
        assert_eq!(artifact.bytes, sidecar.bytes);

        // 4. An independent reader sees the records the live path would have written.
        let parser = bgpkit_parser::BgpkitParser::new(artifact.path.to_str().unwrap()).unwrap();
        let mut announcements = 0;
        for _ in parser {
            announcements += 1;
        }
        assert_eq!(
            announcements, 4,
            "four announcements, two end-of-RIB markers"
        );

        // 5. The scratch root never touched the archive, and is gone.
        assert!(
            !root
                .join("sessions/edge1/0000000000000001/journal.1.bin")
                .exists(),
            "the rebuild does not rotate or append the dead session's journal"
        );
        let stray = std::fs::read_dir(&root).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("bgpviews-republish")
        });
        assert!(!stray, "no scratch archive is left inside the archive root");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The rebuilt path is not this module's invention: a collector fed the same frames for
    /// the same interval publishes the same relative name.
    #[test]
    fn the_rebuilt_name_matches_what_the_live_collector_publishes() {
        let frames = fixture_frames();
        let start = 1_794_702_169_000_000;
        let reference = temp("naming-reference");
        let mut live = Collector::open_at(config(&reference), start).unwrap();
        for (index, frame) in frames.iter().enumerate() {
            live.on_frame(frame, start + index as i64 * 1_000).unwrap();
        }
        live.finish(start + 9_001).unwrap();
        let published = live.published()[0]
            .path
            .strip_prefix(&reference)
            .unwrap()
            .to_path_buf();
        assert_eq!(
            Path::new(&crate::segment::published_name(
                &segment_relative_path("edge1-adj-in-pre", start + 2_000, 900, 1),
                "partial",
                1
            )),
            published,
            "the live collector names its partial segment the way the rebuild does"
        );
        assert_eq!(
            segment_relative_path("edge1-adj-in-pre", start + 2_000, 900, 2),
            "edge1-adj-in-pre/bgpdata/2026.11/UPDATES/updates.20261115.0015.2.gz",
            "a second segment in the interval keeps its ordinal suffix"
        );
        let _ = std::fs::remove_dir_all(&reference);
    }

    #[test]
    fn a_second_republish_is_a_no_op() {
        let root = temp("idempotent");
        let frames = fixture_frames();
        let start = 1_794_702_169_000_000;
        let claim = dead_writer(&root, &frames, start, 1_000);

        let first = match republish(&config(&root), &claim).unwrap() {
            Outcome::Republished(artifact) => artifact,
            other => panic!("expected a rebuild, got {other:?}"),
        };
        let bytes = std::fs::read(&first.path).unwrap();
        let modified = std::fs::metadata(&first.path).unwrap().modified().unwrap();

        let again = republish(&config(&root), &claim).unwrap();
        match again {
            Outcome::AlreadyPublished(path) => assert_eq!(path, first.path),
            other => panic!("expected the claim to be satisfied, got {other:?}"),
        }
        assert_eq!(std::fs::read(&first.path).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&first.path).unwrap().modified().unwrap(),
            modified,
            "an already published artifact is not rewritten"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_claim_whose_journal_is_gone_is_reported_and_publishes_nothing() {
        let root = temp("journal-gone");
        let frames = fixture_frames();
        let start = 1_794_702_169_000_000;
        let claim = dead_writer(&root, &frames, start, 1_000);
        // Retention, or an operator, removed the part the claim's range lives in.
        std::fs::remove_dir_all(root.join("sessions/edge1/0000000000000001")).unwrap();

        match republish(&config(&root), &claim).unwrap() {
            Outcome::Unresolved { reason } => {
                assert!(reason.contains("no journal for epoch 1"), "{reason}");
            }
            other => panic!("expected an unresolved claim, got {other:?}"),
        }
        let relative = segment_relative_path(
            "edge1-adj-in-pre",
            claim.interval_start,
            config(&root).interval_seconds,
            claim.ordinal,
        );
        assert!(!root.join(&relative).exists(), "nothing was published");
        assert!(
            !sidecar_path(&root.join(&relative)).exists(),
            "no sidecar was written either"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A frame the live path refuses is refused by the rebuild too, and the segment stops at
    /// the last good record instead of guessing what the rest of the range meant.
    #[test]
    fn a_refused_frame_stops_the_rebuild_with_the_collectors_reason() {
        let root = temp("refused");
        let frames = fixture_frames();
        let start = 1_794_702_169_000_000;
        let mut writer = Collector::open_at(config(&root), start).unwrap();
        writer.on_frame(&frames[0], start).unwrap();
        writer.on_frame(&frames[1], start + 1_000).unwrap();
        writer.on_frame(&frames[2], start + 2_000).unwrap();
        // The live path journals this frame and then refuses it, because its peer never sent
        // a Peer Up: exactly the state a rebuild has to reproduce, not paper over.
        let refused = with_unknown_peer(&frames[3]);
        assert!(writer.on_frame(&refused, start + 3_000).is_err());
        drop(writer);

        let report = crate::recovery::recover(&root, "edge1-adj-in-pre", "edge1").unwrap();
        assert_eq!(report.claims.len(), 1, "{:?}", report.claims);
        let claim = report.claims[0].clone();

        let artifact = match republish(&config(&root), &claim).unwrap() {
            Outcome::Republished(artifact) => artifact,
            other => panic!("expected a partial rebuild, got {other:?}"),
        };
        assert_eq!(
            artifact.records, 1,
            "the record before the refusal survived"
        );
        assert_eq!(
            artifact.partial_reasons,
            vec!["journal_corrupt".to_string(), "writer_restart".to_string()]
        );
        assert_eq!(read_sidecar(&artifact.sidecar_path).records, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_frame_error_is_named_by_a_reason_the_closed_set_has() {
        assert_eq!(
            refusal(&CollectorError::Frame(FrameError::MissingPerPeerHeader {
                offset: 0
            })),
            Some("parse_error")
        );
        assert_eq!(
            refusal(&CollectorError::MissingPeerUp {
                peer: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            }),
            Some("journal_corrupt")
        );
        assert_eq!(
            refusal(&CollectorError::DiskCritical),
            None,
            "a write failure is not a data refusal"
        );
    }

    /// The fixture's Route Monitoring frame, re-addressed to a peer that never came up.
    fn with_unknown_peer(frame: &[u8]) -> Vec<u8> {
        // A per-peer BMP header carries the monitored peer's address as sixteen bytes at
        // offset 16; the last four hold an IPv4 address.
        let mut patched = frame.to_vec();
        patched[28..32].copy_from_slice(&[172, 31, 250, 99]);
        patched
    }
}
