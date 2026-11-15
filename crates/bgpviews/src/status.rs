//! `status`: what the archive says about itself, read from the archive alone.
//!
//! The rule this command exists to honour: a number here must come from a file on
//! disk, never from process memory. `run` may be serving while `status` reads, so
//! every value is either a published artifact or a durable log line, and the
//! coverage log is read the same way recovery reads it.
//!
//! Design reference: `docs/design.md` section 12.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bgpviews_core::coverage::{RangeKind, Timeline};
use bgpviews_core::segment::Sidecar;

/// One collector's summary.
struct CollectorStatus {
    name: String,
    segments: usize,
    records: u64,
    complete: usize,
    partial: usize,
    withheld: usize,
    reasons: BTreeMap<String, usize>,
    /// `(afi, safi, observed, unknown)` per family, summed over the latest segments.
    baseline: BTreeMap<(u16, u16), (usize, usize)>,
    ranges: usize,
    open_range: Option<String>,
    last_segment: Option<String>,
}

/// Print the archive's status. Returns exit code 1 when a finding is present.
pub fn run(root: &Path) -> i32 {
    let mut collectors: BTreeMap<String, CollectorStatus> = BTreeMap::new();

    // Segments, from the sidecars beside them.
    for sidecar_path in sidecars(root) {
        let Ok(text) = std::fs::read_to_string(&sidecar_path) else {
            continue;
        };
        let Ok(sidecar) = serde_json::from_str::<Sidecar>(&text) else {
            println!(
                "corrupt sidecar: {} (not valid JSON for schema 1)",
                sidecar_path.display()
            );
            continue;
        };
        let entry = collectors
            .entry(sidecar.collector.clone())
            .or_insert_with(|| CollectorStatus {
                name: sidecar.collector.clone(),
                segments: 0,
                records: 0,
                complete: 0,
                partial: 0,
                withheld: 0,
                reasons: BTreeMap::new(),
                baseline: BTreeMap::new(),
                ranges: 0,
                open_range: None,
                last_segment: None,
            });
        entry.segments += 1;
        entry.records += sidecar.records;
        match sidecar.state.as_str() {
            "complete" => entry.complete += 1,
            "withheld" => entry.withheld += 1,
            _ => entry.partial += 1,
        }
        for reason in &sidecar.partial_reasons {
            *entry.reasons.entry(reason.clone()).or_insert(0) += 1;
        }
        for row in &sidecar.baseline {
            let slot = entry.baseline.entry((row.afi, row.safi)).or_insert((0, 0));
            if row.baseline == "observed" {
                slot.0 += 1;
            } else {
                slot.1 += 1;
            }
        }
        entry.last_segment = Some(sidecar.range_start.clone());
    }

    // Coverage, from the log.
    if let Ok(entries) = std::fs::read_dir(root.join("coverage")) {
        for collector_dir in entries.flatten() {
            let path = collector_dir.path().join("updates.jsonl");
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            // DirEntry::file_name returns an OsString, not an Option.
            let name = collector_dir.file_name().to_string_lossy().to_string();
            let (timeline, summary) = Timeline::replay(&bytes);
            let entry = collectors
                .entry(name.clone())
                .or_insert_with(|| CollectorStatus {
                    name: name.clone(),
                    segments: 0,
                    records: 0,
                    complete: 0,
                    partial: 0,
                    withheld: 0,
                    reasons: BTreeMap::new(),
                    baseline: BTreeMap::new(),
                    ranges: 0,
                    open_range: None,
                    last_segment: None,
                });
            entry.ranges = timeline.ranges().len();
            if let Some(open) = timeline.open_range() {
                entry.open_range = Some(match open.kind {
                    RangeKind::Segment => format!("segment {}", open.start.render()),
                    RangeKind::Gap => format!(
                        "gap {} since {}",
                        open.reason.as_deref().unwrap_or("?"),
                        open.start.render()
                    ),
                });
            }
            if summary.discarded_lines > 0 {
                println!(
                    "{}: {} coverage line(s) do not validate; run verify",
                    name, summary.discarded_lines
                );
            }
        }
    }

    if collectors.is_empty() {
        println!("no archive at {}", root.display());
        return 1;
    }

    for status in collectors.values() {
        println!("collector {}", status.name);
        println!(
            "  segments {} (complete {}, partial {}, withheld {}), records {}",
            status.segments, status.complete, status.partial, status.withheld, status.records
        );
        if !status.reasons.is_empty() {
            let reasons: Vec<String> = status
                .reasons
                .iter()
                .map(|(reason, count)| format!("{reason} x{count}"))
                .collect();
            println!("  partial reasons: {}", reasons.join(", "));
        }
        for ((afi, safi), (observed, unknown)) in &status.baseline {
            println!(
                "  family {}/{}: baseline observed {}, unknown {}",
                family_name(*afi),
                safi,
                observed,
                unknown
            );
        }
        println!(
            "  coverage: {} ranges, open {}",
            status.ranges,
            status.open_range.as_deref().unwrap_or("none")
        );
        if let Some(last) = &status.last_segment {
            println!("  last segment starts {}", last);
        }
    }
    0
}

fn family_name(afi: u16) -> &'static str {
    match afi {
        1 => "ipv4",
        2 => "ipv6",
        _ => "other",
    }
}

fn sidecars(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().to_string());
        // Collectors are `<client>-<view>`; the other directories at the root are
        // not segment trees.
        if name.as_deref().is_some_and(|n| {
            matches!(
                n,
                "catalog"
                    | "sessions"
                    | "coverage"
                    | "gaps"
                    | "tombstones"
                    | "work"
                    | "quarantine"
                    | "config"
                    | "clients"
            )
        }) {
            continue;
        }
        collect_sidecars(&path, &mut found);
    }
    found.sort();
    found
}

fn collect_sidecars(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sidecars(&path, found);
        } else if path.extension().is_some_and(|e| e == "json") {
            found.push(path);
        }
    }
}
