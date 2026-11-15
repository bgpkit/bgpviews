//! Print canonical coverage-log lines, with their checksums.
//!
//! This exists so an implementation in another language can be diffed against
//! ours byte for byte without reading our source: run it, and compare the three
//! lines below with what your encoder produces for the same inputs.

use bgpviews_core::coverage::{encode_line, Line, RangeKind, SegmentRef, Stamp, Timeline};

fn stamp(seconds: i64) -> Stamp {
    Stamp::from_parts(seconds, 0).unwrap_or(Stamp(0))
}

fn stamp_micros(seconds: i64, micros: u32) -> Stamp {
    Stamp::from_parts(seconds, micros).unwrap_or(Stamp(0))
}
fn main() {
    let mut out = String::new();
    out.push_str(&encode_line(&Line::Open {
        at: stamp(1_800_000_000),
        range_kind: RangeKind::Gap,
        reason: Some("no_session".into()),
    }));
    out.push_str(&encode_line(&Line::Transition {
        at: stamp_micros(1_800_000_900, 123_456),
        range_kind: RangeKind::Segment,
        reason: None,
        segment: Some(SegmentRef {
            session_epoch: 3,
            ordinal: 1,
        }),
        carried_reasons: vec!["capability_mismatch".into()],
        carried_through_seq: Some(41),
    }));
    out.push_str(&encode_line(&Line::Annotate {
        range: 1,
        reason: "timestamp_anomaly".into(),
    }));
    let (t, s) = Timeline::replay(out.as_bytes());
    print!(
        "{out}@@{}\t{}\t{}",
        s.valid_len,
        s.discarded_lines,
        t.ranges().len()
    );
}
