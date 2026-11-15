#![allow(clippy::expect_used)]
//! Measure replay cost on a synthetic capture of N route records.
//!
//! The record bytes are real: one Route Monitoring frame from the committed lab
//! capture, repeated with fresh receive timestamps. That keeps the measurement
//! about our own path (framing, journal write, envelope, gzip, coverage) rather
//! than about a synthetic shape a router would never send.
//!
//! `cargo run --release --example replay_bench -- 100000`

use std::path::PathBuf;
use std::time::Instant;

use bgpviews_core::bmp::{frames, FrameKind};
use bgpviews_core::replay::{replay_capture, ReplayOptions};

fn main() {
    let count: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100_000);

    let fixture = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../lab/fixtures/frr-8.4-two-router.bin"),
    )
    .expect("read the committed capture");
    let parsed = frames(&fixture).expect("split the capture");

    // One real Route Monitoring frame, plus the Initiation and Peer Up that any
    // session needs before a record can be attributed.
    let preamble: Vec<Vec<u8>> = parsed
        .iter()
        .filter(|f| !matches!(f.kind, FrameKind::RouteMonitoring { .. }))
        .map(|f| f.bytes.clone())
        .collect();
    let record = parsed
        .iter()
        .find_map(|f| match f.kind {
            FrameKind::RouteMonitoring { is_eor: false, .. } => Some(f.bytes.clone()),
            _ => None,
        })
        .expect("the capture has a route announcement");

    let mut capture = Vec::with_capacity(fixture.len() * count / 8);
    for frame in &preamble {
        capture.extend_from_slice(frame);
    }
    for _ in 0..count {
        capture.extend_from_slice(&record);
    }

    let root = std::env::temp_dir().join(format!("bgpviews-bench-{count}"));
    let _ = std::fs::remove_dir_all(&root);
    let options = ReplayOptions::default().with_spacing(1_000);

    let started = Instant::now();
    let report = replay_capture(&root, &capture, &options).expect("replay");
    let elapsed = started.elapsed();

    let bytes_out: u64 = report.segments.iter().map(|s| s.sidecar.bytes).sum();
    let seconds = elapsed.as_secs_f64();
    println!("records        {}", report.records);
    println!("segments       {}", report.segments.len());
    println!("input bytes    {}", capture.len());
    println!("output bytes   {bytes_out}");
    println!("wall seconds   {seconds:.3}");
    println!(
        "records/sec    {:.0}",
        report.records as f64 / seconds.max(1e-9)
    );
    println!(
        "input MB/s     {:.1}",
        capture.len() as f64 / 1_000_000.0 / seconds.max(1e-9)
    );
    println!("archive        {}", root.display());
}
