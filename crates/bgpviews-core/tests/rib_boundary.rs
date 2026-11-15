//! The RIB at an interval boundary, from the collector's own path.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//!
//! `lab/rib_boundary.sh` runs this. The fixture is fed twice, 900 s apart, so the
//! collector closes one segment and opens the next; each close must leave a RIB named
//! after the boundary, readable by the ecosystem's parser.

use std::path::PathBuf;

use bgpviews_core::bmp::StreamSplitter;
use bgpviews_core::collector::{Collector, CollectorConfig};

fn config(root: &std::path::Path) -> CollectorConfig {
    CollectorConfig {
        client: "r2".to_string(),
        view: "adj-in-pre".to_string(),
        epoch: 1,
        interval_seconds: 900,
        root: root.to_path_buf(),
        rib: true,
    }
}

fn frames() -> Vec<Vec<u8>> {
    // The workspace root is two levels up from `crates/bgpviews-core`.
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../lab/fixtures/frr-8.4-two-router.bin");
    let bytes = std::fs::read(path).expect("fixture");
    let mut splitter = StreamSplitter::new();
    splitter.push(&bytes);
    let mut out = Vec::new();
    while let Some(frame) = splitter.next_frame().expect("frame") {
        out.push(frame);
    }
    out
}

#[test]
fn a_closed_interval_leaves_a_rib_named_after_its_boundary() {
    let dir = std::env::temp_dir().join(format!("bgpviews-rib-boundary-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("root");

    // 1763166000 is a 900 s boundary; the fixture's own capture time sits just inside it.
    let start = 1_763_166_169_i64 * 1_000_000;
    let mut collector = Collector::open_at(config(&dir), start).expect("collector");
    let frames = frames();
    for frame in &frames {
        collector.on_frame(frame, start).expect("first pass");
    }
    // The same traffic one interval later closes the first interval.
    let later = start + 900 * 1_000_000;
    for frame in &frames {
        collector.on_frame(frame, later).expect("second pass");
    }
    collector.finish(later).expect("finish");

    let snapshots = collector.rib_snapshots();
    assert!(
        snapshots.len() >= 2,
        "expected a RIB per closed interval, got {}",
        snapshots.len()
    );

    for snapshot in snapshots {
        assert!(
            snapshot.path.to_string_lossy().contains("/RIBS/"),
            "a RIB belongs under RIBS/: {}",
            snapshot.path.display()
        );
        // `PublishedSegment::path` is relative to the archive root, the same way the
        // sidecar records it, so a consumer joins it with the root it opened.
        let path = if snapshot.path.is_absolute() {
            snapshot.path.clone()
        } else {
            dir.join(&snapshot.path)
        };
        let parser = bgpkit_parser::BgpkitParser::new(path.to_str().expect("utf-8"))
            .expect("the RIB must parse with bgpkit-parser");
        let prefixes: Vec<String> = parser
            .into_elem_iter()
            .map(|elem| elem.prefix.prefix.to_string())
            .collect();
        assert!(
            prefixes.iter().any(|p| p == "192.0.2.0/24"),
            "the RIB should hold the announced prefix, got {prefixes:?}"
        );
        assert!(
            prefixes.iter().any(|p| p == "2001:db8::/32"),
            "the RIB should hold the IPv6 prefix too, got {prefixes:?}"
        );
    }

    // The first snapshot names the boundary that closed, not the moment the next
    // record happened to arrive: 1763166000 + 900 = 1763166600.
    let boundary = (start / 1_000_000).div_euclid(900) * 900 + 900;
    assert_eq!(
        snapshots[0].sidecar.range_start,
        bgpviews_core::segment::render_stamp(boundary * 1_000_000),
        "a boundary RIB is named after the boundary it summarizes"
    );

    // And the RIB keeps the in-memory view honest.
    assert_eq!(
        collector.rib().prefixes(),
        4,
        "four prefixes were announced"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
