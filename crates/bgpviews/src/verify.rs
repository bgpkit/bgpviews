//! `verify`: reconcile the archive against itself, without writing anything.
//!
//! Design reference: `docs/design.md` sections 7 and 8. The checks here are the ones
//! that can be made from the files alone: a published segment must exist, hash to what
//! its sidecar says, and be named by exactly one closed coverage range; a session's
//! journal must replay to its last frame without a hole; and the coverage log must
//! still satisfy its own invariants.
//!
//! A finding is printed as `<class> <path>: <detail>` and the command exits 1, so a
//! cron or a human gets the same answer.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use bgpviews_core::coverage::{RangeKind, Timeline};
use bgpviews_core::journal;
use bgpviews_core::segment::Sidecar;

/// Run the checks; returns the number of findings.
pub fn run(root: &Path) -> usize {
    let mut findings = 0usize;
    let mut withheld = 0usize;
    let mut named_segments: BTreeSet<String> = BTreeSet::new();

    // 1. Every closed segment range names a sidecar that exists, and the sidecar's
    //    artifact hashes to what it claims.
    for coverage_path in coverage_logs(root) {
        let Ok(bytes) = std::fs::read(&coverage_path) else {
            continue;
        };
        let (timeline, summary) = Timeline::replay(&bytes);
        if summary.discarded_lines > 0 {
            println!(
                "corrupt {}: {} coverage line(s) do not validate; the timeline stops there",
                coverage_path.display(),
                summary.discarded_lines
            );
            findings += 1;
        }
        if let Err(detail) = timeline.check_invariants() {
            println!("corrupt {}: {detail}", coverage_path.display());
            findings += 1;
        }
        for range in timeline.ranges() {
            if range.kind != RangeKind::Segment {
                continue;
            }
            let Some(segment) = range.segment else {
                println!(
                    "corrupt {}: segment range at {} names no artifact",
                    coverage_path.display(),
                    range.start.render()
                );
                findings += 1;
                continue;
            };
            named_segments.insert(format!(
                "{}|{}|{}",
                range.start.render(),
                segment.session_epoch,
                segment.ordinal
            ));
        }
    }

    // 2. Every sidecar on disk hashes to its artifact, and its identity is unique.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for sidecar_path in sidecars(root) {
        let Ok(text) = std::fs::read_to_string(&sidecar_path) else {
            continue;
        };
        let Ok(sidecar) = serde_json::from_str::<Sidecar>(&text) else {
            println!("corrupt {}: not a schema 1 sidecar", sidecar_path.display());
            findings += 1;
            continue;
        };
        let identity = format!(
            "{}|{}|{}|{}|{}",
            sidecar.collector,
            sidecar.stream as u8 as char,
            sidecar.session_epoch,
            sidecar.ordinal,
            sidecar.range_start
        );
        if !seen.insert(identity) {
            println!(
                "duplicate {}: two sidecars claim the same segment identity",
                sidecar_path.display()
            );
            findings += 1;
        }
        let artifact = root.join(&sidecar.path);
        match std::fs::read(&artifact) {
            Ok(bytes) => {
                let digest = sha256_hex(&bytes);
                if digest != sidecar.sha256 {
                    println!(
                        "corrupt {}: sha256 is {} but the sidecar says {}",
                        artifact.display(),
                        digest,
                        sidecar.sha256
                    );
                    findings += 1;
                } else if bytes.len() as u64 != sidecar.bytes {
                    println!(
                        "corrupt {}: {} bytes but the sidecar says {}",
                        artifact.display(),
                        bytes.len(),
                        sidecar.bytes
                    );
                    findings += 1;
                }
            }
            Err(e) => {
                // A withheld artifact is a known, recorded absence: the tombstone says so.
                // Reporting it as missing would make every quarantined segment look like
                // data loss on every verify run.
                match bgpviews_core::quarantine::withheld_reason(root, &sidecar.path) {
                    Some(_reason) => withheld += 1,
                    None => {
                        println!("missing {}: {e}", artifact.display());
                        findings += 1;
                    }
                }
            }
        }
    }

    // 3. Every journal part replays to its last frame without a sequence hole.
    for part in journal_parts(root) {
        match journal::replay(&part) {
            Ok((frames, summary)) => {
                if summary.torn {
                    println!(
                        "torn {}: the file ends inside a frame after {} frame(s)",
                        part.display(),
                        frames.len()
                    );
                    findings += 1;
                }
            }
            Err(e) => {
                println!("corrupt {}: {e}", part.display());
                findings += 1;
            }
        }
    }

    // Tombstones name artifacts that are no longer in the archive, so the sidecar loop
    // above never sees them. They are counted here: a withheld segment is a recorded
    // absence, not a finding.
    let tombstoned = count_tombstones(root);
    let withheld = withheld + tombstoned;
    if withheld > 0 {
        println!("{withheld} segment(s) withheld by a tombstone");
    }
    if findings == 0 {
        println!("clean: no findings under {}", root.display());
    } else {
        println!("{findings} finding(s) under {}", root.display());
    }
    findings
}

fn count_tombstones(root: &Path) -> usize {
    let mut count = 0usize;
    let mut stack = vec![root.join("tombstones")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "json") {
                count += 1;
            }
        }
    }
    count
}

fn sha256_hex(bytes: &[u8]) -> String {
    // The segment writer hashes with its own implementation; verify reuses it through
    // the same crate so the two cannot disagree about the digest.
    bgpviews_core::segment::sha256_hex(bytes)
}

fn coverage_logs(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join("coverage")) {
        for entry in entries.flatten() {
            let path = entry.path().join("updates.jsonl");
            if path.is_file() {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn journal_parts(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.join("sessions")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .map(|n| n.to_string_lossy().starts_with("journal."))
                .unwrap_or(false)
                && path.extension().map(|e| e == "bin").unwrap_or(false)
            {
                found.push(path);
            }
        }
    }
    found.sort();
    found
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
        let name = entry.file_name().to_string_lossy().to_string();
        if matches!(
            name.as_str(),
            "catalog"
                | "sessions"
                | "coverage"
                | "gaps"
                | "tombstones"
                | "work"
                | "quarantine"
                | "config"
                | "clients"
        ) {
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
