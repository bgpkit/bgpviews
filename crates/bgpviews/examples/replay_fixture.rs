//! Replay the committed lab capture into a directory and print what came out.
//!
//! `cargo run --example replay_fixture -- /tmp/bgpviews-replay`
#![allow(clippy::expect_used)]

use std::path::PathBuf;

use bgpviews_core::replay::{replay_capture, ReplayOptions};

fn main() {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("bgpviews-replay"));
    let capture = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../lab/fixtures/frr-8.4-two-router.bin"),
    )
    .expect("read the committed capture");

    let report = replay_capture(&root, &capture, &ReplayOptions::default()).expect("replay");
    println!("archive root   {}", root.display());
    println!("collector      {}", report.collector);
    println!("journal        {}", report.journal.display());
    println!("segment        {}", report.segment.display());
    println!("sidecar        {}", report.sidecar.display());
    println!("coverage       {}", report.coverage.display());
    println!("frames         {}", report.frames);
    println!("records        {}", report.records);
    println!("end of rib     {}", report.end_of_rib);
    println!(
        "range          {} .. {}",
        report.sidecar_fields.range_start, report.sidecar_fields.range_end
    );
    println!("bytes          {}", report.sidecar_fields.bytes);
    println!("sha256         {}", report.sidecar_fields.sha256);
}
