//! The coverage log: the file that answers "what covers what, and why not".
//!
//! Design reference: `docs/design.md` sections 5.4 and 5.5. This module is the
//! encoder, the replay and the invariant checker for that file; it is not a
//! policy layer. Policy (when to open a gap, what reason applies) belongs to the
//! collector, which is not written yet.
//!
//! One line is one change, and the line is the unit of durability:
//!
//! ```text
//! <json> <crc32c as 8 lowercase hex digits>\n
//! ```
//!
//! The checksum covers the JSON bytes only, so a reader can validate a line
//! without knowing where the next one starts. A line whose checksum does not
//! validate is not part of the timeline: recovery truncates the file there, and
//! a reader ignores it.

use serde::{Deserialize, Serialize};

use crate::crc::crc32c;

/// A receive time in microseconds since the Unix epoch.
///
/// Microseconds, not nanoseconds: the design renders every timestamp as
/// `<seconds>.<six digits>`, and one unit everywhere avoids the class of bug
/// where a value is compared in one unit and printed in another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Stamp(pub i64);

impl Stamp {
    /// Build a stamp from whole seconds and a microsecond remainder.
    ///
    /// # Errors
    /// Returns an error when the remainder is outside `0..1_000_000`, because a
    /// stamp that renders ambiguously cannot be compared with one that does not.
    pub fn from_parts(seconds: i64, micros: u32) -> Result<Self, LogError> {
        if micros >= 1_000_000 {
            return Err(LogError::BadStamp { micros });
        }
        let micros_i64 = i64::from(micros);
        let total = seconds
            .checked_mul(1_000_000)
            .and_then(|scaled| scaled.checked_add(micros_i64))
            .ok_or(LogError::StampOverflow)?;
        Ok(Self(total))
    }

    /// The rendered form the design specifies: `<seconds>.<six digits>`.
    #[must_use]
    pub fn render(self) -> String {
        let secs = self.0.div_euclid(1_000_000);
        let micros = self.0.rem_euclid(1_000_000);
        format!("{secs}.{micros:06}")
    }
}

/// What a range asserts about the time it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RangeKind {
    /// An MRT file covers this range. It may hold zero records.
    Segment,
    /// No file covers this range, and `reason` says why.
    Gap,
}

/// Which MRT artifact a segment range names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentRef {
    /// The session epoch that produced the segment.
    pub session_epoch: u64,
    /// The ordinal within its interval, starting at 1.
    pub ordinal: u32,
}

/// One change to the timeline, which is what a line in the coverage log holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "line", rename_all = "snake_case")]
pub enum Line {
    /// The first range of an empty file.
    Open {
        /// When the range starts.
        at: Stamp,
        /// What the range asserts.
        range_kind: RangeKind,
        /// Why, for a gap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Close the open range at `at` and open a successor that starts there.
    Transition {
        /// The boundary: the closed range ends here and the successor starts here.
        at: Stamp,
        /// What the successor asserts.
        range_kind: RangeKind,
        /// Why, for a gap successor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Which artifact, for a segment successor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        segment: Option<SegmentRef>,
        /// Reasons carried into the successor from a refusal that landed on a gap.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        carried_reasons: Vec<String>,
        /// The greatest view-state sequence consumed by those carried reasons.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        carried_through_seq: Option<u64>,
    },
    /// Add a reason to the open segment range.
    Annotate {
        /// The range being annotated. Must be the open one.
        range: u32,
        /// The reason added.
        reason: String,
    },
}

/// A range as replayed from the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    /// Index in the timeline, assigned by replay and stable for the file's life.
    pub id: u32,
    /// Start of the half-open range.
    pub start: Stamp,
    /// End, once the range closed.
    pub end: Option<Stamp>,
    /// What the range asserts.
    pub kind: RangeKind,
    /// Why, for a gap.
    pub reason: Option<String>,
    /// Which artifact, for a segment.
    pub segment: Option<SegmentRef>,
    /// Reasons carried in at open plus any added by `annotate`, in the order added.
    pub reasons: Vec<String>,
}

impl Range {
    fn open(id: u32, start: Stamp, kind: RangeKind, reason: Option<String>) -> Self {
        Self {
            id,
            start,
            end: None,
            kind,
            reason,
            segment: None,
            reasons: Vec::new(),
        }
    }
}

/// Why a log could not be read or a change could not be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogError {
    /// A timestamp rendered with a microsecond field outside `0..1_000_000`.
    BadStamp {
        /// The offending value.
        micros: u32,
    },
    /// A timestamp that cannot be represented in microseconds.
    StampOverflow,
    /// The JSON itself was malformed.
    Json(String),
    /// `open` on a timeline that already has a range.
    AlreadyOpen,
    /// `open` asking for a segment, which cannot name its artifact: a timeline
    /// starts as a gap and reaches a segment through `transition`.
    OpenCannotBeSegment,
    /// A change that needs an open range, on a timeline that has none.
    NoOpenRange,
    /// A boundary earlier than the start of the range it closes.
    BackwardsBoundary {
        /// The boundary supplied.
        at: Stamp,
        /// The start of the range it would close.
        start: Stamp,
    },
    /// A segment range with no artifact reference.
    MissingSegmentRef,
    /// A gap range with no reason.
    MissingReason,
    /// `annotate` against a range that is not the open one.
    NotOpenRange {
        /// The range named.
        asked: u32,
        /// The range that is actually open.
        open: u32,
    },
    /// `annotate` against a gap, which has no artifact to annotate.
    AnnotateOnGap {
        /// The range named.
        range: u32,
    },
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadStamp { micros } => {
                write!(f, "microsecond field {micros} is outside 0..1000000")
            }
            Self::StampOverflow => write!(f, "timestamp does not fit in microseconds"),
            Self::Json(e) => write!(f, "malformed coverage line: {e}"),
            Self::AlreadyOpen => write!(f, "the timeline already has a range; use transition"),
            Self::OpenCannotBeSegment => write!(
                f,
                "open cannot create a segment: a timeline starts as a gap and a segment is reached by transition"
            ),
            Self::NoOpenRange => write!(f, "the timeline has no open range; use open"),
            Self::BackwardsBoundary { at, start } => write!(
                f,
                "boundary {} precedes the start {} of the range it would close",
                at.render(),
                start.render()
            ),
            Self::MissingSegmentRef => write!(f, "a segment range must name its artifact"),
            Self::MissingReason => write!(f, "a gap range must carry a reason"),
            Self::NotOpenRange { asked, open } => {
                write!(f, "range {asked} is not the open range ({open})")
            }
            Self::AnnotateOnGap { range } => {
                write!(f, "range {range} is a gap and has no artifact to annotate")
            }
        }
    }
}

impl std::error::Error for LogError {}

/// A replayed timeline plus the facts recovery needs to report.
#[derive(Debug, Clone, Default)]
pub struct Timeline {
    ranges: Vec<Range>,
    /// Bytes up to the end of the last valid line.
    valid_len: usize,
    /// Lines that failed their checksum, and were therefore not applied.
    discarded_lines: usize,
}

/// What recovery found at the end of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplaySummary {
    /// Bytes that belong to the timeline. A file longer than this was truncated.
    pub valid_len: usize,
    /// The number of trailing lines discarded, at most one for a torn append.
    pub discarded_lines: usize,
}

impl Timeline {
    /// Replay a coverage log.
    ///
    /// A line whose checksum does not validate ends the timeline: everything
    /// after it is ignored and reported through [`ReplaySummary`]. This is
    /// deliberate rather than convenient, because a reader that skipped a bad
    /// line and continued would produce a timeline with a hole it could not see.
    #[must_use]
    pub fn replay(bytes: &[u8]) -> (Self, ReplaySummary) {
        let mut timeline = Self::default();
        let mut offset = 0usize;
        let mut discarded = 0usize;

        while offset < bytes.len() {
            let Some(nl) = bytes[offset..].iter().position(|b| *b == b'\n') else {
                // No terminator: a partial line, which recovery truncates.
                discarded += 1;
                break;
            };
            let line_end = offset + nl;
            let raw = &bytes[offset..line_end];
            match decode_line(raw) {
                Ok(line) => {
                    // A well-formed line that the model rejects is a corrupt
                    // timeline, not a torn append; stop and let verify report it.
                    if timeline.apply(&line).is_err() {
                        discarded += 1;
                        break;
                    }
                    offset = line_end + 1;
                    timeline.valid_len = offset;
                }
                Err(_) => {
                    discarded += 1;
                    break;
                }
            }
        }

        timeline.discarded_lines = discarded;
        let summary = ReplaySummary {
            valid_len: timeline.valid_len,
            discarded_lines: discarded,
        };
        (timeline, summary)
    }

    /// Apply one line.
    ///
    /// # Errors
    /// Returns [`LogError`] when the line would break contiguity, name a range
    /// that is not open, or leave a range without the field its kind requires.
    pub fn apply(&mut self, line: &Line) -> Result<(), LogError> {
        match line {
            Line::Open {
                at,
                range_kind,
                reason,
            } => {
                if !self.ranges.is_empty() {
                    return Err(LogError::AlreadyOpen);
                }
                if *range_kind == RangeKind::Segment {
                    return Err(LogError::OpenCannotBeSegment);
                }
                check_shape(*range_kind, reason.as_deref(), None)?;
                let mut range = Range::open(0, *at, *range_kind, reason.clone());
                range.reasons.extend(reason.clone());
                self.ranges.push(range);
                Ok(())
            }
            Line::Transition {
                at,
                range_kind,
                reason,
                segment,
                carried_reasons,
                ..
            } => {
                let Some(tail) = self.ranges.last_mut() else {
                    return Err(LogError::NoOpenRange);
                };
                if tail.end.is_some() {
                    return Err(LogError::NoOpenRange);
                }
                if *at < tail.start {
                    return Err(LogError::BackwardsBoundary {
                        at: *at,
                        start: tail.start,
                    });
                }
                tail.end = Some(*at);
                check_shape(*range_kind, reason.as_deref(), *segment)?;
                let id = self.ranges.len() as u32;
                let mut range = Range::open(id, *at, *range_kind, reason.clone());
                range.segment = *segment;
                if let Some(r) = reason {
                    range.reasons.push(r.clone());
                }
                range.reasons.extend(carried_reasons.iter().cloned());
                self.ranges.push(range);
                Ok(())
            }
            Line::Annotate { range, reason } => {
                let Some(tail) = self.ranges.last_mut() else {
                    return Err(LogError::NoOpenRange);
                };
                if tail.end.is_some() {
                    return Err(LogError::NoOpenRange);
                }
                if tail.id != *range {
                    return Err(LogError::NotOpenRange {
                        asked: *range,
                        open: tail.id,
                    });
                }
                if tail.kind != RangeKind::Segment {
                    return Err(LogError::AnnotateOnGap { range: *range });
                }
                tail.reasons.push(reason.clone());
                Ok(())
            }
        }
    }

    /// The ranges, in order.
    #[must_use]
    pub fn ranges(&self) -> &[Range] {
        &self.ranges
    }

    /// The open range, if any.
    #[must_use]
    pub fn open_range(&self) -> Option<&Range> {
        self.ranges.last().filter(|r| r.end.is_none())
    }

    /// The summary a caller needs to truncate a file or report a torn append.
    #[must_use]
    pub fn summary(&self) -> ReplaySummary {
        ReplaySummary {
            valid_len: self.valid_len,
            discarded_lines: self.discarded_lines,
        }
    }

    /// Check the invariants of section 5.4 over the whole timeline.
    ///
    /// # Errors
    /// Returns a human-readable violation naming the first range that breaks an
    /// invariant, so `verify` can point at a file rather than at a paragraph.
    pub fn check_invariants(&self) -> Result<(), String> {
        if self.ranges.is_empty() {
            return Err("timeline has no ranges".to_string());
        }
        let mut open = 0usize;
        for (index, range) in self.ranges.iter().enumerate() {
            if range.end.is_none() {
                open += 1;
                if index + 1 != self.ranges.len() {
                    return Err(format!("range {index} is open but is not the last range"));
                }
            } else if let Some(next) = self.ranges.get(index + 1) {
                if range.end != Some(next.start) {
                    return Err(format!(
                        "range {index} ends at {:?} but range {} starts at {}",
                        range.end.map(Stamp::render),
                        next.id,
                        next.start.render()
                    ));
                }
            }
        }
        if open != 1 {
            return Err(format!(
                "timeline has {open} open ranges, expected exactly 1"
            ));
        }
        for range in &self.ranges {
            if range.kind == RangeKind::Segment && range.segment.is_none() {
                return Err(format!("segment range {} names no artifact", range.id));
            }
            if range.kind == RangeKind::Gap && range.reason.is_none() {
                return Err(format!("gap range {} has no reason", range.id));
            }
        }
        Ok(())
    }
}

fn check_shape(
    kind: RangeKind,
    reason: Option<&str>,
    segment: Option<SegmentRef>,
) -> Result<(), LogError> {
    match kind {
        RangeKind::Segment => {
            if segment.is_none() {
                return Err(LogError::MissingSegmentRef);
            }
        }
        RangeKind::Gap => {
            if reason.is_none() {
                return Err(LogError::MissingReason);
            }
        }
    }
    Ok(())
}

/// Encode one line, including its checksum and terminator.
#[must_use]
pub fn encode_line(line: &Line) -> String {
    // `serde_json` on a value built entirely from owned fields cannot fail, and
    // the shape is asserted by the round-trip test below.
    let json = serde_json::to_string(line).unwrap_or_else(|_| String::from("{}"));
    let sum = crc32c(json.as_bytes());
    format!("{json} {sum:08x}\n")
}

/// Decode one line, without its terminator.
///
/// # Errors
/// Returns [`LogError::Json`] when the line is not `<json> <8 hex digits>` with a
/// checksum that matches the JSON bytes.
pub fn decode_line(raw: &[u8]) -> Result<Line, LogError> {
    let text = std::str::from_utf8(raw).map_err(|e| LogError::Json(e.to_string()))?;
    let (json, sum_text) = text
        .rsplit_once(' ')
        .ok_or_else(|| LogError::Json("no checksum field".to_string()))?;
    if sum_text.len() != 8 {
        return Err(LogError::Json(format!(
            "checksum field is {} characters, expected 8",
            sum_text.len()
        )));
    }
    let expected = u32::from_str_radix(sum_text, 16)
        .map_err(|e| LogError::Json(format!("checksum is not hex: {e}")))?;
    if crc32c(json.as_bytes()) != expected {
        return Err(LogError::Json("checksum mismatch".to_string()));
    }
    serde_json::from_str(json).map_err(|e| LogError::Json(e.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn stamp(secs: i64) -> Stamp {
        Stamp::from_parts(secs, 0).unwrap_or(Stamp(0))
    }

    fn gap(secs: i64, reason: &str) -> Line {
        Line::Transition {
            at: stamp(secs),
            range_kind: RangeKind::Gap,
            reason: Some(reason.to_string()),
            segment: None,
            carried_reasons: Vec::new(),
            carried_through_seq: None,
        }
    }

    fn segment(secs: i64, epoch: u64, ordinal: u32) -> Line {
        Line::Transition {
            at: stamp(secs),
            range_kind: RangeKind::Segment,
            reason: None,
            segment: Some(SegmentRef {
                session_epoch: epoch,
                ordinal,
            }),
            carried_reasons: Vec::new(),
            carried_through_seq: None,
        }
    }

    #[test]
    fn line_round_trips_through_text() {
        let line = segment(1_800_000_000, 7, 3);
        let text = encode_line(&line);
        assert!(text.ends_with('\n'));
        let body = text.trim_end_matches('\n');
        assert_eq!(decode_line(body.as_bytes()).unwrap(), line);
    }

    #[test]
    fn checksum_rejects_a_flipped_byte() {
        let line = gap(1_800_000_000, "no_session");
        let mut text = encode_line(&line).into_bytes();
        text[4] ^= 0x01;
        let body = text.strip_suffix(b"\n").unwrap_or(&text).to_vec();
        assert!(decode_line(&body).is_err());
    }

    #[test]
    fn replay_walks_ranges_and_keeps_them_contiguous() {
        let log = [
            encode_line(&Line::Open {
                at: stamp(100),
                range_kind: RangeKind::Gap,
                reason: Some("no_session".to_string()),
            }),
            encode_line(&gap(200, "no_session")),
            encode_line(&segment(300, 1, 1)),
        ]
        .concat();

        let (timeline, summary) = Timeline::replay(log.as_bytes());
        assert_eq!(summary.discarded_lines, 0);
        assert_eq!(summary.valid_len, log.len());
        assert_eq!(timeline.ranges().len(), 3);
        assert_eq!(timeline.ranges()[0].start, stamp(100));
        assert_eq!(timeline.ranges()[0].end, Some(stamp(200)));
        assert_eq!(timeline.ranges()[2].start, stamp(300));
        assert!(timeline.ranges()[2].end.is_none());
        assert!(timeline.check_invariants().is_ok());
    }

    #[test]
    fn a_torn_tail_is_discarded_and_reported() {
        let good = encode_line(&Line::Open {
            at: stamp(100),
            range_kind: RangeKind::Gap,
            reason: Some("no_session".to_string()),
        });
        let mut bytes = good.clone().into_bytes();
        bytes.extend_from_slice(b"{\"line\":\"transition\",\"at\":\"20");

        let (timeline, summary) = Timeline::replay(&bytes);
        assert_eq!(timeline.ranges().len(), 1);
        assert_eq!(summary.valid_len, good.len());
        assert_eq!(summary.discarded_lines, 1);
        assert!(timeline.check_invariants().is_ok());
    }

    #[test]
    fn a_bad_line_stops_the_timeline_instead_of_leaving_a_hole() {
        let first = encode_line(&Line::Open {
            at: stamp(100),
            range_kind: RangeKind::Gap,
            reason: Some("no_session".to_string()),
        });
        let broken = "{\"line\":\"transition\"} deadbeef\n";
        let third = encode_line(&segment(300, 1, 1));
        let log = [first.clone(), broken.to_string(), third].concat();

        let (timeline, summary) = Timeline::replay(log.as_bytes());
        assert_eq!(
            timeline.ranges().len(),
            1,
            "the lines after the bad one are not applied"
        );
        assert_eq!(summary.valid_len, first.len());
        assert_eq!(summary.discarded_lines, 1);
    }

    #[test]
    fn transition_needs_an_open_range() {
        let mut timeline = Timeline::default();
        let err = timeline.apply(&gap(200, "no_session")).unwrap_err();
        assert!(matches!(err, LogError::NoOpenRange));
    }

    #[test]
    fn open_is_rejected_once_a_range_exists() {
        let mut timeline = Timeline::default();
        timeline
            .apply(&Line::Open {
                at: stamp(100),
                range_kind: RangeKind::Gap,
                reason: Some("no_session".to_string()),
            })
            .unwrap();
        let err = timeline
            .apply(&Line::Open {
                at: stamp(150),
                range_kind: RangeKind::Gap,
                reason: Some("no_session".to_string()),
            })
            .unwrap_err();
        assert!(matches!(err, LogError::AlreadyOpen));
    }

    #[test]
    fn a_backwards_boundary_is_refused() {
        let mut timeline = Timeline::default();
        timeline
            .apply(&Line::Open {
                at: stamp(300),
                range_kind: RangeKind::Gap,
                reason: Some("no_session".to_string()),
            })
            .unwrap();
        let err = timeline.apply(&gap(200, "no_session")).unwrap_err();
        assert!(matches!(err, LogError::BackwardsBoundary { .. }));
    }

    #[test]
    fn annotate_lands_on_the_open_segment_and_nowhere_else() {
        let mut timeline = Timeline::default();
        let open_segment = timeline.apply(&Line::Open {
            at: stamp(100),
            range_kind: RangeKind::Segment,
            reason: None,
        });
        assert!(matches!(open_segment, Err(LogError::OpenCannotBeSegment)));
        timeline
            .apply(&Line::Open {
                at: stamp(100),
                range_kind: RangeKind::Gap,
                reason: Some("view_not_observed".to_string()),
            })
            .ok();
        let on_gap = timeline
            .apply(&Line::Annotate {
                range: 0,
                reason: "capability_mismatch".to_string(),
            })
            .unwrap_err();
        assert!(matches!(on_gap, LogError::AnnotateOnGap { .. }));
    }

    #[test]
    fn carried_reasons_land_on_the_opened_range() {
        let mut timeline = Timeline::default();
        timeline
            .apply(&Line::Open {
                at: stamp(100),
                range_kind: RangeKind::Gap,
                reason: Some("peer_syncing_at_open".to_string()),
            })
            .unwrap();
        timeline
            .apply(&Line::Transition {
                at: stamp(200),
                range_kind: RangeKind::Segment,
                reason: None,
                segment: Some(SegmentRef {
                    session_epoch: 1,
                    ordinal: 1,
                }),
                carried_reasons: vec!["capability_mismatch".to_string()],
                carried_through_seq: Some(9),
            })
            .unwrap();
        let tail = timeline.open_range();
        assert!(tail.is_some(), "the transition left no open range");
        if let Some(tail) = tail {
            assert_eq!(tail.reasons, vec!["capability_mismatch".to_string()]);
        }
    }

    #[test]
    fn invariants_reject_a_segment_without_an_artifact() {
        let mut timeline = Timeline::default();
        timeline
            .apply(&Line::Open {
                at: stamp(100),
                range_kind: RangeKind::Gap,
                reason: Some("no_session".to_string()),
            })
            .unwrap();
        // Bypass the line-level check to prove the invariant check stands alone.
        timeline
            .ranges
            .push(Range::open(1, stamp(200), RangeKind::Segment, None));
        assert!(timeline.check_invariants().is_err());
    }

    #[test]
    fn stamps_render_and_order_as_the_design_specifies() {
        let s = Stamp::from_parts(1_800_000_000, 1234).unwrap_or(Stamp(0));
        assert_eq!(s.render(), "1800000000.001234");
        assert!(Stamp::from_parts(1, 1_000_000).is_err());
        assert!(stamp(200) > stamp(100));
    }

    /// The exact bytes of one line, so a second implementation can be diffed
    /// against this one instead of against a description of it.
    #[test]
    fn golden_line_bytes() {
        let line = Line::Open {
            at: Stamp::from_parts(1_800_000_000, 0).unwrap(),
            range_kind: RangeKind::Gap,
            reason: Some("no_session".to_string()),
        };
        let expected = "{\"line\":\"open\",\"at\":1800000000000000,\"range_kind\":\"gap\",\"reason\":\"no_session\"} b4fdb3e7\n";
        assert_eq!(encode_line(&line), expected);
        // And it must decode back to the same value from those exact bytes.
        assert_eq!(
            decode_line(expected.trim_end_matches('\n').as_bytes()).unwrap(),
            line
        );
    }
}
