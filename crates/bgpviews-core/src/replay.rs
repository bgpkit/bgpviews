//! Replaying a captured BMP stream into an archive directory.
//!
//! This is the same state machine the live collector runs: `replay_capture` builds
//! a [`Collector`], feeds it the capture's frames with synthetic receive times, and
//! reports what it published. That is deliberate. A separate replay loop would be a
//! second implementation of the coverage and segment rules, and the first time the
//! two disagreed the archive would mean two different things depending on how the
//! bytes arrived.
//!
//! What a replay cannot exercise, because it needs a socket: backpressure, disk
//! guards, clock steps, and gap closure while a view is idle. Those belong to `run`
//! and are listed in `docs/experiments.md`.

use std::path::{Path, PathBuf};

use crate::bmp::{frames, FrameError};
use crate::collector::{Collector, CollectorConfig, CollectorError, PublishedSegment};
use crate::segment::Sidecar;

/// A published segment, re-exported so a replay's caller does not depend on the
/// collector module for a type it only reads.
pub type SegmentOut = PublishedSegment;

/// What a replay needs to know that a capture cannot tell it.
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// Configured client name.
    pub client: String,
    /// The view the capture came from.
    pub view: String,
    /// Session epoch to record.
    pub epoch: u64,
    /// Synthetic receive time for the first frame, in microseconds. A capture
    /// carries no receive clock, so the replay invents one and says so.
    pub receive_start_micros: i64,
    /// How far apart to space frames, in microseconds.
    pub frame_spacing_micros: i64,
    /// Interval length in seconds, which decides segment names and bounds.
    pub interval_seconds: i64,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            client: "edge1".to_string(),
            view: "adj-in-pre".to_string(),
            epoch: 1,
            // 2026-11-15T00:22:49Z, the second the committed lab capture was taken.
            receive_start_micros: 1_794_702_169 * 1_000_000,
            frame_spacing_micros: 1_000,
            interval_seconds: 900,
        }
    }
}

impl ReplayOptions {
    /// Space frames this far apart, which makes a short capture cross an interval
    /// boundary. Used by the rollover experiment.
    #[must_use]
    pub fn with_spacing(mut self, micros: i64) -> Self {
        self.frame_spacing_micros = micros;
        self
    }

    /// Use this interval length, in seconds.
    #[must_use]
    pub fn with_interval(mut self, seconds: i64) -> Self {
        self.interval_seconds = seconds;
        self
    }

    /// The collector name, `<client>-<view>`.
    #[must_use]
    pub fn collector(&self) -> String {
        format!("{}-{}", self.client, self.view)
    }
}

/// Why a replay failed.
#[derive(Debug)]
pub enum ReplayError {
    /// The capture could not be split.
    Frame(FrameError),
    /// The collector refused a frame or failed to write an artifact.
    Collector(CollectorError),
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame(e) => write!(f, "{e}"),
            Self::Collector(e) => write!(f, "{e}"),
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
    /// The coverage log.
    pub coverage: PathBuf,
    /// The first published MRT segment.
    pub segment: PathBuf,
    /// Its sidecar.
    pub sidecar: PathBuf,
    /// Every published segment, in order.
    pub segments: Vec<SegmentOut>,
    /// Frames accepted.
    pub frames: u64,
    /// MRT records written across all segments.
    pub records: u64,
    /// Of those, end-of-RIB markers.
    pub end_of_rib: u64,
    /// Frames for a view this collector does not archive.
    pub unconfigured_view: u64,
    /// The range the replay covers, in microseconds.
    pub range: (i64, i64),
    /// The sidecar of the first segment, for a caller that wants the hash.
    pub sidecar_fields: Sidecar,
}

/// Replay `capture` into `root`, which is created if it does not exist.
///
/// # Errors
/// Returns [`ReplayError`] when the capture cannot be split, a record names a peer
/// that never came up, the interval is not a whole number of minutes, or an
/// artifact cannot be written.
pub fn replay_capture(
    root: &Path,
    capture: &[u8],
    options: &ReplayOptions,
) -> Result<ReplayReport, ReplayError> {
    let parsed = frames(capture).map_err(ReplayError::Frame)?;
    let config = CollectorConfig {
        client: options.client.clone(),
        view: options.view.clone(),
        epoch: options.epoch,
        interval_seconds: options.interval_seconds,
        rib: false,
        disk_warning_percent: 90,
        disk_critical_percent: 97,
        write_stall_ms: 2000,
        backpressure_deadline_seconds: 300,
        root: root.to_path_buf(),
    };
    let collector_name = config.collector();
    let mut collector =
        Collector::open_at(config, options.receive_start_micros).map_err(ReplayError::Collector)?;

    let mut receive_at = Vec::with_capacity(parsed.len());
    for (index, frame) in parsed.iter().enumerate() {
        let at = options.receive_start_micros + index as i64 * options.frame_spacing_micros;
        receive_at.push(at);
        collector
            .on_frame(&frame.bytes, at)
            .map_err(ReplayError::Collector)?;
    }
    let last = receive_at
        .last()
        .copied()
        .unwrap_or(options.receive_start_micros);
    collector.finish(last).map_err(ReplayError::Collector)?;

    let segments = collector.published().to_vec();
    let first = segments.first().cloned().ok_or_else(|| {
        ReplayError::Collector(CollectorError::Coverage(
            "the capture published no segment".to_string(),
        ))
    })?;

    Ok(ReplayReport {
        collector: collector_name,
        journal: collector.journal_path(),
        coverage: collector.coverage_path(),
        segment: first.path,
        sidecar: first.sidecar_path,
        segments,
        frames: collector.frames(),
        records: collector.records(),
        end_of_rib: collector.end_of_rib(),
        unconfigured_view: collector.unconfigured_view(),
        range: (first.interval_start, last),
        sidecar_fields: first.sidecar,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_short_interval_is_reported_as_the_collectors_error() {
        let root = std::env::temp_dir().join("bgpviews-replay-short-interval");
        let _ = std::fs::remove_dir_all(&root);
        let err = replay_capture(&root, &[], &ReplayOptions::default().with_interval(30))
            .expect_err("thirty seconds is not a whole number of minutes");
        assert!(matches!(
            err,
            ReplayError::Collector(CollectorError::InvalidInterval { seconds: 30 })
        ));
    }

    #[test]
    fn options_name_the_collector_as_client_dash_view() {
        let options = ReplayOptions::default();
        assert_eq!(options.collector(), "edge1-adj-in-pre");
    }
}
