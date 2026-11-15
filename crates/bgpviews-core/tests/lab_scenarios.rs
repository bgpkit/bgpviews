//! The lab scenarios, replayed: churn and session restart.
//!
//! Each fixture is a real capture from `lab/scenario.sh`. The assertions are about
//! what the archive must say given those events, not about the bytes: two
//! withdrawals must appear as records, a restart must close one segment and open
//! another in the same interval, and the coverage log must stay contiguous.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use bgpkit_parser::BgpkitParser;
use bgpviews_core::coverage::{RangeKind, Timeline};
use bgpviews_core::replay::{replay_capture, ReplayOptions};

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../lab/fixtures/{name}"));
    std::fs::read(path).expect("the scenario fixture is committed")
}

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("bgpviews-scenario-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    root
}

#[test]
fn churn_produces_withdrawals_in_the_segment() {
    let root = temp_root("churn");
    let report =
        replay_capture(&root, &fixture("churn.bin"), &ReplayOptions::default()).expect("replay");
    assert_eq!(report.segments.len(), 1, "a single interval");
    let segment = &report.segments[0];
    assert_eq!(
        segment.sidecar.records, 10,
        "8 from the dump plus 2 withdrawals"
    );

    let parser = BgpkitParser::new(segment.path.to_str().expect("utf-8"))
        .expect("bgpkit-parser opens it")
        .into_elem_iter();
    let elems: Vec<_> = parser.collect();
    let announcements = elems
        .iter()
        .filter(|e| e.elem_type == bgpkit_parser::models::ElemType::ANNOUNCE)
        .count();
    let withdrawals = elems.len() - announcements;
    assert_eq!(announcements, 4);
    assert_eq!(withdrawals, 2);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_restart_closes_the_segment_and_opens_the_next_ordinal() {
    let root = temp_root("restart");
    let report =
        replay_capture(&root, &fixture("restart.bin"), &ReplayOptions::default()).expect("replay");
    assert_eq!(report.segments.len(), 2, "the restart splits the interval");
    assert_eq!(report.segments[0].sidecar.ordinal, 1);
    assert_eq!(report.segments[1].sidecar.ordinal, 2);
    assert!(report.segments[0]
        .sidecar
        .partial_reasons
        .contains(&"peer_restarted".to_string()));
    assert!(report.segments[0]
        .sidecar
        .partial_reasons
        .contains(&"peer_vanished".to_string()));
    assert!(report.segments[0]
        .path
        .to_string_lossy()
        .ends_with("0015.gz"));
    assert!(report.segments[1]
        .path
        .to_string_lossy()
        .ends_with("0015.2.gz"));

    // The coverage log covers the same time as the two segments, contiguously.
    let text = std::fs::read_to_string(&report.coverage).expect("coverage");
    let (timeline, summary) = Timeline::replay(text.as_bytes());
    assert_eq!(summary.discarded_lines, 0);
    timeline
        .check_invariants()
        .expect("contiguous, one open range");
    let segments: Vec<_> = timeline
        .ranges()
        .iter()
        .filter(|r| r.kind == RangeKind::Segment)
        .collect();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].segment.map(|s| s.ordinal), Some(1));
    assert_eq!(segments[1].segment.map(|s| s.ordinal), Some(2));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_interval_shorter_than_a_minute_is_refused() {
    let root = temp_root("short-interval");
    let err = replay_capture(
        &root,
        &fixture("churn.bin"),
        &ReplayOptions::default().with_interval(1),
    )
    .expect_err("a one-second interval would collide on minute names");
    assert!(matches!(
        err,
        bgpviews_core::replay::ReplayError::InvalidInterval { seconds: 1 }
    ));
}
