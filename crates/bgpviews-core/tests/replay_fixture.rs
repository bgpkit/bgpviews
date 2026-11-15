//! The proof the design asks for: a captured BMP stream becomes a directory that
//! an independent reader can parse.
//!
//! The reader here is `bgpkit-parser`, deliberately: if our writer and our reader
//! shared code, a round trip would prove only that they agree with each other.
//! This test writes, then reads with the same library the rest of BGPKIT uses.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use bgpkit_parser::BgpkitParser;
use bgpviews_core::replay::{replay_capture, ReplayOptions};

fn fixture() -> Vec<u8> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../lab/fixtures/frr-8.4-two-router.bin");
    std::fs::read(path).expect("the lab fixture is committed")
}

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("bgpviews-replay-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp dir");
    root
}

#[test]
fn a_capture_becomes_a_segment_an_mrt_reader_can_parse() {
    let root = temp_root("parse");
    let options = ReplayOptions::default();
    let report = replay_capture(&root, &fixture(), &options).expect("replay succeeds");

    assert_eq!(report.collector, "edge1-adj-in-pre");
    assert!(report.segment.exists(), "the segment was published");
    assert!(report.sidecar.exists(), "the sidecar was written beside it");
    assert!(report.coverage.exists(), "the coverage log was written");
    assert_eq!(report.records, 8, "eight route records, including EoR");
    assert_eq!(report.end_of_rib, 4);

    let parser = BgpkitParser::new(report.segment.to_str().expect("utf-8 path"))
        .expect("bgpkit-parser opens the file we wrote");
    let mut prefixes = Vec::new();
    let mut as_paths: Vec<Vec<u32>> = Vec::new();
    let mut count = 0usize;
    for elem in parser {
        count += 1;
        prefixes.push(elem.prefix.to_string());
        let hops: Vec<u32> = elem
            .as_path
            .as_ref()
            .map(|path| {
                let mut hops = Vec::new();
                for segment in &path.segments {
                    match segment {
                        bgpkit_parser::models::AsPathSegment::AsSequence(seq) => {
                            hops.extend(seq.iter().map(|a| u32::from(*a)));
                        }
                        bgpkit_parser::models::AsPathSegment::AsSet(set) => {
                            hops.extend(set.iter().map(|a| u32::from(*a)));
                        }
                        // Confederation segments do not appear in an eBGP
                        // announcement between these routers; skip them rather
                        // than flatten them into the hop count.
                        bgpkit_parser::models::AsPathSegment::ConfedSequence(_)
                        | bgpkit_parser::models::AsPathSegment::ConfedSet(_) => {}
                    }
                }
                hops
            })
            .unwrap_or_default();
        as_paths.push(hops);
    }
    // An end-of-RIB marker carries no prefix, so the element count is the four
    // announcements, not the eight records. bgpdump and monocle agree on 4.
    assert_eq!(count, 4, "the reader sees every announcement we wrote");
    assert!(
        prefixes.contains(&"192.0.2.0/24".to_string()),
        "prefixes: {prefixes:?}"
    );
    assert!(prefixes.contains(&"198.51.100.0/24".to_string()));
    assert!(prefixes.contains(&"203.0.113.0/24".to_string()));
    assert!(prefixes.contains(&"2001:db8::/32".to_string()));

    // The AS path is the measured FRR one, two hops because the exporter prepends
    // its own AS before exporting. Asserting it here keeps the caveat honest.
    assert!(
        as_paths.iter().any(|p| p == &vec![65002, 65001]),
        "expected a 65002 65001 path, got {as_paths:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_sidecar_hash_matches_the_published_file() {
    let root = temp_root("hash");
    let report = replay_capture(&root, &fixture(), &ReplayOptions::default()).expect("replay");
    let bytes = std::fs::read(&report.segment).expect("read the published segment");
    assert_eq!(bytes.len() as u64, report.sidecar_fields.bytes);
    // The digest the sidecar carries must be the digest of the file on disk, which
    // is what `sha256sum` would print.
    let output = std::process::Command::new("sha256sum")
        .arg(&report.segment)
        .output()
        .expect("sha256sum is available");
    let printed = String::from_utf8_lossy(&output.stdout);
    let digest = printed.split_whitespace().next().unwrap_or_default();
    assert_eq!(digest, report.sidecar_fields.sha256);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn every_frame_reaches_the_journal_in_order() {
    let root = temp_root("journal");
    let report = replay_capture(&root, &fixture(), &ReplayOptions::default()).expect("replay");
    let (frames, summary) =
        bgpviews_core::journal::replay(&report.journal).expect("journal replays");
    assert_eq!(frames.len(), 10, "ten frames in, ten frames journaled");
    assert!(!summary.torn);
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(frame.seq, index as u64 + 1);
        assert_eq!(frame.payload[0], 3, "each frame is BMPv3");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_coverage_log_covers_exactly_the_range_the_segment_does() {
    let root = temp_root("coverage");
    let report = replay_capture(&root, &fixture(), &ReplayOptions::default()).expect("replay");
    let text = std::fs::read_to_string(&report.coverage).expect("read coverage");
    let (timeline, summary) = bgpviews_core::coverage::Timeline::replay(text.as_bytes());
    assert_eq!(summary.discarded_lines, 0);
    timeline.check_invariants().expect("invariants hold");

    let ranges = timeline.ranges();
    // The timeline starts as a gap (nothing is covered before the first record),
    // transitions into the segment range, and closes with a gap where the capture
    // ended. Three ranges, contiguous, one open.
    assert_eq!(ranges.len(), 3, "gap, segment, gap");
    assert_eq!(ranges[0].reason.as_deref(), Some("no_session"));
    let segment = &ranges[1];
    assert_eq!(segment.kind, bgpviews_core::coverage::RangeKind::Segment);
    let start = segment.start.render();
    let end = segment.end.expect("the segment range is closed").render();
    assert_eq!(start, report.sidecar_fields.range_start);
    assert_eq!(segment.segment.map(|s| s.ordinal), Some(1));
    // Each range starts where the previous one ended, by construction.
    assert_eq!(ranges[2].start.render(), end);
    assert_eq!(ranges[2].reason.as_deref(), Some("session_ended"));
    assert_eq!(ranges[1].end.map(|e| e.render()), Some(end.clone()));
    let _ = std::fs::remove_dir_all(&root);
}
