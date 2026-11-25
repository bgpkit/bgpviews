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
    // A root that does not exist, or holds nothing this program wrote, reconciles trivially:
    // every set is empty and every loop is skipped. Reporting that as `clean` tells an
    // operator their archive is fine when they pointed verify at the wrong path.
    if !root.is_dir() {
        out!(
            "missing {}: the archive root does not exist",
            root.display()
        );
        return 1;
    }
    let looks_like_an_archive = root.join("coverage").is_dir() || root.join("sessions").is_dir();
    if !looks_like_an_archive {
        out!(
            "missing {}: no coverage/ or sessions/ directory, so this is not a bgpviews archive",
            root.display()
        );
        return 1;
    }
    let mut withheld = 0usize;
    let mut named_segments: BTreeSet<String> = BTreeSet::new();
    // Segment identities the coverage log claims, and the ones the archive actually has.
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    let mut present: BTreeSet<String> = BTreeSet::new();
    // The last range of a live collector is the interval its writer is still filling. Its sidecar
    // appears when that interval closes, so counting it as a claim with no artifact reports the
    // normal state of a serving archive as a finding.
    let mut open_tail: BTreeSet<String> = BTreeSet::new();

    // 1. Every closed segment range names a sidecar that exists, and the sidecar's
    //    artifact hashes to what it claims.
    for coverage_path in coverage_logs(root) {
        let Ok(bytes) = std::fs::read(&coverage_path) else {
            continue;
        };
        let (timeline, summary) = Timeline::replay(&bytes);
        // The coverage file lives at coverage/<collector>/updates.jsonl, and the collector is
        // part of a segment's identity: omitting it lets one collector's coordinates mask
        // another's missing segment.
        let collector = coverage_path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if summary.discarded_lines > 0 {
            out!(
                "corrupt {}: {} coverage line(s) do not validate; the timeline stops there",
                coverage_path.display(),
                summary.discarded_lines
            );
            findings += 1;
        }
        if let Err(detail) = timeline.check_invariants() {
            out!("corrupt {}: {detail}", coverage_path.display());
            findings += 1;
        }
        for range in timeline.ranges() {
            if range.kind != RangeKind::Segment {
                continue;
            }
            let Some(segment) = range.segment else {
                out!(
                    "corrupt {}: segment range at {} names no artifact",
                    coverage_path.display(),
                    range.start.render()
                );
                findings += 1;
                continue;
            };
            let identity = format!(
                "{}|{}|{}|{}",
                collector,
                range.start.render(),
                segment.session_epoch,
                segment.ordinal
            );
            claimed.insert(identity.clone());
            named_segments.insert(identity);
        }
        if let Some(range) = timeline.ranges().last() {
            if let (RangeKind::Segment, Some(segment)) = (range.kind, range.segment) {
                open_tail.insert(format!(
                    "{}|{}|{}|{}",
                    collector,
                    range.start.render(),
                    segment.session_epoch,
                    segment.ordinal
                ));
            }
        }
    }

    // 2. Every sidecar on disk hashes to its artifact, and its identity is unique.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // The catalog's `segments` key, one per sidecar: (collector, stream, interval start, ordinal).
    let mut expected_rows: BTreeSet<String> = BTreeSet::new();
    for sidecar_path in sidecars(root) {
        let Ok(text) = std::fs::read_to_string(&sidecar_path) else {
            continue;
        };
        let Ok(sidecar) = serde_json::from_str::<Sidecar>(&text) else {
            out!("corrupt {}: not a schema 1 sidecar", sidecar_path.display());
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
        // A RIB snapshot is named by its cutoff, not by a coverage range: the coverage log
        // describes the update stream. Counting it as present made every RIB an orphan on a
        // default `rib = true` archive, which is the archive's normal shape.
        let is_ribs = matches!(sidecar.stream, bgpviews_core::segment::Stream::Ribs);
        expected_rows.insert(format!(
            "{}|{}|{}|{}",
            sidecar.collector,
            if is_ribs { "ribs" } else { "updates" },
            sidecar.range_start,
            sidecar.ordinal
        ));
        if !is_ribs {
            // Inserted before the withheld branch: a quarantined segment still satisfies the
            // claim that names it, which is what "withheld" means rather than "lost".
            present.insert(format!(
                "{}|{}|{}|{}",
                sidecar.collector, sidecar.range_start, sidecar.session_epoch, sidecar.ordinal
            ));
        }
        if sidecar.state == "withheld" {
            // The bytes were withheld because they were wrong. Checking their digest against
            // the sidecar would report the defect the tombstone already records, on every run,
            // forever; the tombstone is where that judgement lives.
            withheld += 1;
            continue;
        }
        if !seen.insert(identity) {
            out!(
                "duplicate {}: two sidecars claim the same segment identity",
                sidecar_path.display()
            );
            findings += 1;
        }
        // A quarantined sidecar keeps the path it had while published, so the pair is found
        // under quarantine/<collector>/ rather than at that path.
        let artifact = {
            let published = root.join(&sidecar.path);
            if published.exists() {
                published
            } else {
                let name = std::path::Path::new(&sidecar.path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                root.join("quarantine").join(&sidecar.collector).join(name)
            }
        };
        match std::fs::read(&artifact) {
            Ok(bytes) => {
                let digest = sha256_hex(&bytes);
                if digest != sidecar.sha256 {
                    out!(
                        "corrupt {}: sha256 is {} but the sidecar says {}",
                        artifact.display(),
                        digest,
                        sidecar.sha256
                    );
                    findings += 1;
                } else if bytes.len() as u64 != sidecar.bytes {
                    out!(
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
                        out!("missing {}: {e}", artifact.display());
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
                    out!(
                        "torn {}: the file ends inside a frame after {} frame(s)",
                        part.display(),
                        frames.len()
                    );
                    findings += 1;
                }
            }
            Err(e) => {
                out!("corrupt {}: {e}", part.display());
                findings += 1;
            }
        }
    }

    // 3b. A peer-state log whose tail was cut is a recorded absence, not a corrupted file: the
    // versions in the cut are gone and the `.torn` file beside it is the evidence. It is counted
    // as a finding because an operator asking "was this peer synced" needs to know the answer is
    // incomplete, and a file nobody looks for is not a record.
    for log in peer_state_logs(root) {
        match bgpviews_core::peer_state::is_torn(&log) {
            Ok(true) => {
                out!(
                    "torn {}: the log ends inside a frame and recovery has not cut it yet",
                    log.display()
                );
                findings += 1;
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                out!("corrupt {}: {e}", log.display());
                findings += 1;
                continue;
            }
        }
        match bgpviews_core::peer_state::replay(&log) {
            Ok(versions) => {
                let torn = log.with_file_name("peer_state.torn");
                if torn.is_file() {
                    out!(
                        "torn {}: {} version(s) survived and the bytes after them are in {}",
                        log.display(),
                        versions.len(),
                        torn.display()
                    );
                    findings += 1;
                }
            }
            Err(e) => {
                out!("corrupt {}: {e}", log.display());
                findings += 1;
            }
        }
    }

    // Tombstones name artifacts that are no longer in the archive, so the sidecar loop
    // above never sees them. They are counted here: a withheld segment is a recorded
    // absence, not a finding.
    // A sidecar-less MRT is invisible to the loops above: `present` is built from sidecars, so a
    // collector file whose description was lost (a rename that never completed, a hand-copied
    // tree, a disk that dropped the `.json`) is never seen at all. Section 8 defines an orphan as
    // a collector MRT *or* sidecar with nothing describing it, so the files are scanned too.
    let mut undescribed: Vec<PathBuf> = Vec::new();
    for file in mrt_files(root) {
        let sidecar = file.with_extension(format!(
            "{}.json",
            file.extension()
                .map(|ext| ext.to_string_lossy().to_string())
                .unwrap_or_default()
        ));
        if !sidecar.is_file() {
            undescribed.push(file);
        }
    }
    for path in &undescribed {
        out!(
            "orphan {}: this MRT has no sidecar, so nothing describes what it holds",
            path.display()
        );
    }
    findings += undescribed.len();

    // The catalog is a projection of the sidecars, so the two have to name the same segments.
    // A row without a sidecar is a stale projection, and a sidecar without a row is a projection
    // that a reader following the catalog cannot see; section 8 makes both findings rather than
    // something the next rebuild is expected to paper over.
    // The catalog is a projection, so an archive that never ran `rebuild-index` is not in
    // error: there is nothing to reconcile against. The check runs once the `segments` table
    // exists; gating on the catalog directory alone reported a mid-rebuild archive, and an
    // archive whose catalog was written before quarantined segments were projected, as broken.
    if root.join("catalog").join("segments").is_dir() {
        let catalogued = match bgpviews_core::catalog::segment_keys(root) {
            Ok(keys) => keys,
            Err(e) => {
                out!("corrupt catalog: {e}");
                findings += 1;
                std::collections::BTreeSet::new()
            }
        };
        for key in catalogued.difference(&expected_rows) {
            out!("catalog_stale {key}: the catalog names a segment no sidecar describes");
            findings += 1;
        }
        for key in expected_rows.difference(&catalogued) {
            out!("catalog_missing {key}: the archive has a segment the catalog does not name");
            findings += 1;
        }
    }

    let orphans: Vec<String> = present.difference(&claimed).cloned().collect();
    let dangling: Vec<String> = claimed.difference(&present).cloned().collect();
    let tombstoned = count_tombstones(root);
    // A tombstone without a sidecar is a quarantine that was interrupted before the move, or
    // one whose files someone removed by hand. Count those, but do not double-count the pair.
    let mut withheld = withheld.max(tombstoned);
    // Both directions, not one. The loop above asks "does the coverage log point at a real
    // artifact"; this asks "is every artifact pointed at". A segment nobody claims is as much
    // a defect as a claim with no segment, and only one of the two was checked before.
    for identity in &orphans {
        out!(
            "orphan {}: no closed coverage range names this segment",
            identity
        );
    }
    findings += orphans.len();
    // A claim whose artifact was quarantined is a recorded absence: the tombstone names the
    // path and the reason. Counting it as missing made the documented repair unverifiable,
    // because quarantine moves the sidecar along with the artifact.
    let mut dangling_real = 0usize;
    for identity in &dangling {
        let path = identity.rsplit_once('|').map(|(rest, _)| rest);
        let known = path.is_some_and(|rest| {
            let mut parts = rest.splitn(2, '|');
            let collector = parts.next().unwrap_or_default();
            let start = parts.next().unwrap_or_default();
            withheld_reason_for(root, collector, start)
        });
        if known {
            withheld += 1;
            continue;
        }
        if open_tail.contains(identity) {
            out!(
                "open {}: the interval this writer is still filling",
                identity
            );
            continue;
        }
        out!(
            "missing {}: the coverage log names a segment with no sidecar",
            identity
        );
        dangling_real += 1;
    }
    findings += dangling_real;

    if withheld > 0 {
        out!("{withheld} segment(s) withheld by a tombstone");
    }
    if findings == 0 {
        out!("clean: no findings under {}", root.display());
    } else {
        out!("{findings} finding(s) under {}", root.display());
    }
    findings
}

/// Whether a tombstone exists for a segment of `collector` starting at `range_start`.
fn withheld_reason_for(root: &Path, collector: &str, range_start: &str) -> bool {
    let dir = root.join("tombstones").join(collector);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if text.contains(range_start) && text.contains("\"state\": \"withheld\"") {
            return true;
        }
    }
    false
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

/// Every `peer_state.log` in the archive.
fn peer_state_logs(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(clients) = std::fs::read_dir(root.join("sessions")) else {
        return found;
    };
    for client in clients.flatten() {
        let Ok(epochs) = std::fs::read_dir(client.path()) else {
            continue;
        };
        for epoch in epochs.flatten() {
            let log = epoch.path().join(bgpviews_core::peer_state::LOG_NAME);
            if log.is_file() {
                found.push(log);
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

/// Every MRT file in a collector's `bgpdata` tree, temporary files excluded.
///
/// A `.tmp` is a part being written, and a `.partial.gz` is a part that was published as partial:
/// both are legitimate names, and only the sidecar question applies to them.
fn mrt_files(root: &Path) -> Vec<PathBuf> {
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
            "catalog" | "sessions" | "coverage" | "gaps" | "tombstones" | "work" | "config"
        ) {
            continue;
        }
        collect_mrt(&path, &mut found);
    }
    found
}

/// Walk one collector subtree for `.gz` files that are not temp files.
fn collect_mrt(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_mrt(&path, found);
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".gz") && !name.ends_with(".tmp") {
            found.push(path);
        }
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
        let name = entry.file_name().to_string_lossy().to_string();
        if matches!(
            name.as_str(),
            "catalog"
                | "sessions"
                | "coverage"
                | "gaps"
                | "tombstones"
                | "work"
                | "config"
                | "clients"
        ) {
            continue;
        }
        // `quarantine/` is walked on purpose: a withheld artifact keeps its sidecar, so its
        // coverage claim is still accounted for. Skipping it made the documented repair
        // (quarantine a bad segment, then verify) report `missing` forever.
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_peer_state_log_is_a_finding_not_a_silence() {
        let root =
            std::env::temp_dir().join(format!("bgpviews-verify-peer-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let dir = root.join("sessions").join("edge1").join("0000000000000001");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join(bgpviews_core::peer_state::LOG_NAME);
        std::fs::write(&log, bgpviews_core::peer_state::MAGIC).unwrap();

        // An empty log with no tear reconciles: one magic and no frames.
        assert_eq!(run(&root), 0, "a whole log is not a finding");

        // The cut suffix beside it is the record that versions were lost, and an operator who
        // asks whether a peer was synced has to be told the answer is incomplete.
        std::fs::write(dir.join("peer_state.torn"), b"half a frame").unwrap();
        assert_eq!(run(&root), 1, "a cut log is reported");
        let _ = std::fs::remove_dir_all(&root);
    }
}
