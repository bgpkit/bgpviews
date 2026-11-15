//! Replay the committed lab capture into a directory and print what came out.
//!
//! `cargo run --example replay_fixture -- /tmp/bgpviews-replay`
#![allow(clippy::expect_used)]

use std::path::PathBuf;

use bgpviews_core::replay::{replay_capture, ReplayOptions};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut interval = 900i64;
    let mut spacing = 1_000i64;
    let mut capture_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../lab/fixtures/frr-8.4-two-router.bin");
    let mut positional: Vec<String> = Vec::new();
    while let Some(arg) = args.first().cloned() {
        args.remove(0);
        match arg.as_str() {
            "--interval" => {
                if let Some(value) = args.first() {
                    interval = value.parse().unwrap_or(interval);
                    args.remove(0);
                }
            }
            "--spacing" => {
                if let Some(value) = args.first() {
                    spacing = value.parse().unwrap_or(spacing);
                    args.remove(0);
                }
            }
            "--capture" => {
                if let Some(value) = args.first() {
                    capture_path = PathBuf::from(value);
                    args.remove(0);
                }
            }
            other => positional.push(other.to_string()),
        }
    }
    let root = positional
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("bgpviews-replay"));
    let capture = std::fs::read(&capture_path).expect("read the capture");

    let options = ReplayOptions::default()
        .with_interval(interval)
        .with_spacing(spacing);
    let report = replay_capture(&root, &capture, &options).expect("replay");
    println!("archive root   {}", root.display());
    println!("collector      {}", report.collector);
    println!("journal        {}", report.journal.display());
    println!("capture        {}", capture_path.display());
    println!("interval       {interval}s, frame spacing {spacing}us");
    println!("segments       {}", report.segments.len());
    for segment in &report.segments {
        println!(
            "  {}  records={} state={} reasons={:?}",
            segment.path.display(),
            segment.sidecar.records,
            segment.sidecar.state,
            segment.sidecar.partial_reasons
        );
    }
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
