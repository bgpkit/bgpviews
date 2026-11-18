//! Recovery: what a collector must fix before it may write anything new.
//!
//! Design reference: `docs/design.md` section 11. A crashed collector leaves three kinds
//! of debris, and each needs a different answer because each is a different claim about
//! the world.
//!
//! 1. **A torn journal tail.** The bytes after the last complete frame were never
//!    acknowledged, so they are discarded. The surviving prefix is the record.
//! 2. **An open coverage range.** The timeline says records were still being placed when
//!    the process died. It cannot stay open across a restart, because a second writer
//!    would then be extending a range nobody owns. It is closed at the last frame the
//!    journal proves, with the reason `writer_restart`.
//! 3. **An unpublished segment.** A segment is published by renaming a temporary file, so
//!    a crash leaves `.partial` and `.tmp` files behind and never a half-published
//!    segment. They are removed: they were never in the archive.
//! 4. **A torn coverage line.** The append that wrote it did not finish, so the bytes after
//!    the last validating line are cut. Without that, the log would stay unreadable forever:
//!    no command rewrites coverage, and every start would refuse the archive.
//!
//! Recovery never invents records and never extends a range to "now". The only times it
//! writes are times the journal already contains.

use std::path::{Path, PathBuf};

use crate::coverage::{encode_line, Line, RangeKind, Stamp, Timeline};
use crate::journal;

/// What recovery did, in the order a human would read it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Whether a coverage range was open and had to be closed.
    pub closed_range: Option<String>,
    /// The last frame the journal proves, rendered, when one was found.
    pub last_frame: Option<String>,
    /// Temporary files removed.
    pub removed: Vec<PathBuf>,
    /// Journal parts whose tail was torn.
    pub torn_parts: Vec<PathBuf>,
    /// Segments the coverage log claims and no artifact satisfies, so a restart has to
    /// rebuild them before `verify` can be clean.
    pub claims: Vec<UnpublishedClaim>,
    /// Coverage lines cut from the log because they did not validate.
    pub repaired_lines: usize,
    /// Bytes those lines occupied.
    pub dropped_bytes: usize,
    /// Files this recovery could not fix, with the reason.
    pub problems: Vec<String>,
}

/// A segment the coverage log claims but no artifact was published for.
///
/// The dead writer opened the segment, then died before the rename published it, so the claim
/// is real and the file is not. Design section 11 says such a segment is rebuilt as partial
/// with `writer_restart`, which is why the claim carries the identity it must be rebuilt under
/// rather than a fresh one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnpublishedClaim {
    /// Interval start, in microseconds: the segment's own name depends on it.
    pub interval_start: i64,
    /// Where the claim ends, one microsecond after the last frame the journal proves.
    pub until: i64,
    /// The session epoch the claim names.
    pub session_epoch: u64,
    /// The ordinal the claim names, which must not be handed out again.
    pub ordinal: u32,
}

/// Bring `root` back to a state a new writer may extend.
///
/// `collector` is `<client>-<view>`; `client` selects the journal directory.
///
/// # Errors
/// Returns a message when the coverage log exists but cannot be read or appended to. A
/// missing log is not an error: a collector that never ran has nothing to recover.
pub fn recover(root: &Path, collector: &str, client: &str) -> Result<Report, String> {
    let mut report = Report::default();

    // 1. Journal parts: find the last frame this archive can prove, and report torn tails.
    let mut last: Option<i64> = None;
    for part in journal_parts(root, client) {
        match journal::replay(&part) {
            Ok((frames, summary)) => {
                if summary.torn {
                    report.torn_parts.push(part.clone());
                }
                if let Some(frame) = frames.last() {
                    let at = frame.seconds * 1_000_000 + i64::from(frame.micros);
                    last = Some(last.map_or(at, |current: i64| current.max(at)));
                }
            }
            Err(e) => report.problems.push(format!("{}: {e}", part.display())),
        }
    }
    if let Some(at) = last {
        report.last_frame = Some(crate::segment::render_stamp(at));
    }

    // 2. Temporary files: never part of the archive, so removing them changes nothing a
    // reader could have seen.
    for path in temporary_files(root) {
        match std::fs::remove_file(&path) {
            Ok(()) => report.removed.push(path),
            Err(e) => report.problems.push(format!("{}: {e}", path.display())),
        }
    }

    // 3. The coverage range: close it at the last proven frame, never at "now".
    let log = root.join("coverage").join(collector).join("updates.jsonl");
    let Ok(bytes) = std::fs::read(&log) else {
        return Ok(report);
    };
    let (timeline, summary) = Timeline::replay(&bytes);
    if summary.discarded_lines > 0 {
        // A torn tail is the one repair a restart can make without inventing a record: cut
        // the file back to the longest prefix that validates, so the next append follows a
        // known-good line instead of the archive refusing to open for good.
        if summary.valid_len == 0 {
            report.problems.push(format!(
                "{}: not one line validates, so there is no prefix to keep",
                log.display()
            ));
            return Ok(report);
        }
        let truncated = std::fs::OpenOptions::new()
            .write(true)
            .open(&log)
            .and_then(|file| {
                file.set_len(summary.valid_len as u64)?;
                file.sync_all()
            });
        if let Err(e) = truncated {
            report.problems.push(format!("{}: {e}", log.display()));
            return Ok(report);
        }
        report.repaired_lines = summary.discarded_lines;
        report.dropped_bytes = bytes.len() - summary.valid_len;
    }
    let Some(open) = timeline.open_range() else {
        return Ok(report);
    };
    // An open gap is a truthful statement that nothing has been recorded, and a restart
    // continues it without ambiguity. An open segment is the opposite: it claims records
    // were being placed by a writer that no longer exists. Only the segment needs closing,
    // which is also what makes recovery idempotent.
    if open.kind == RangeKind::Gap {
        return Ok(report);
    }
    let Some(from) = last else {
        // Nothing proves when the range ended, and closing it at an invented time would
        // put a made-up boundary in the one structure that exists to be audited.
        report.problems.push(format!(
            "{}: an open range cannot be closed because no journal frame proves a time",
            log.display()
        ));
        return Ok(report);
    };
    // One microsecond after the last proven frame: the range covers what was written,
    // and the successor starts where the record stops.
    let at = from + 1;
    if let Some(segment) = open.segment {
        report.claims.push(UnpublishedClaim {
            interval_start: open.start.0,
            until: at,
            session_epoch: segment.session_epoch,
            ordinal: segment.ordinal,
        });
    }
    let line = Line::Transition {
        at: Stamp::from_parts(at.div_euclid(1_000_000), at.rem_euclid(1_000_000) as u32)
            .map_err(|e| format!("{}: {e}", log.display()))?,
        range_kind: RangeKind::Gap,
        reason: Some("writer_restart".to_string()),
        segment: None,
        carried_reasons: Vec::new(),
        carried_through_seq: None,
    };
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .map_err(|e| format!("{}: {e}", log.display()))?;
    use std::io::Write;
    file.write_all(encode_line(&line).as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("{}: {e}", log.display()))?;
    report.closed_range = Some("segment".to_string());
    Ok(report)
}

fn journal_parts(root: &Path, client: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.join("sessions").join(client)];
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
            {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn temporary_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name != "sessions" && name != "coverage" {
                    stack.push(path);
                }
            } else {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".partial") || name.ends_with(".tmp") {
                    found.push(path);
                }
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::coverage::SegmentRef;

    fn stamp(micros: i64) -> Stamp {
        Stamp::from_parts(micros / 1_000_000, (micros % 1_000_000) as u32).unwrap()
    }

    #[test]
    fn an_open_segment_is_closed_at_the_last_journal_frame() {
        let dir = std::env::temp_dir().join(format!("bgpviews-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("coverage/r2-adj-in-pre")).unwrap();
        std::fs::create_dir_all(dir.join("sessions/r2/0000000000000001")).unwrap();

        // A journal with two frames, the second a full second later.
        let mut writer = crate::journal::JournalWriter::create(
            &dir.join("sessions/r2/0000000000000001/journal.0.bin"),
        )
        .unwrap();
        writer.append(1_763_166_169, 0, 0, b"frame one").unwrap();
        writer.append(1_763_166_170, 0, 0, b"frame two").unwrap();
        writer.sync().unwrap();
        drop(writer);

        // Coverage that still believes a segment is open, plus two temp files.
        let log = dir.join("coverage/r2-adj-in-pre/updates.jsonl");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            encode_line(&Line::Open {
                at: stamp(1_763_166_169_000_000),
                range_kind: RangeKind::Gap,
                reason: Some("no_session".to_string()),
            })
            .as_bytes(),
        );
        bytes.extend_from_slice(
            encode_line(&Line::Transition {
                at: stamp(1_763_166_169_000_000),
                range_kind: RangeKind::Segment,
                reason: None,
                segment: Some(SegmentRef {
                    session_epoch: 1,
                    ordinal: 1,
                }),
                carried_reasons: Vec::new(),
                carried_through_seq: None,
            })
            .as_bytes(),
        );
        std::fs::write(&log, &bytes).unwrap();
        std::fs::create_dir_all(dir.join("r2-adj-in-pre/bgpdata/2026.11/UPDATES")).unwrap();
        std::fs::write(
            dir.join("r2-adj-in-pre/bgpdata/2026.11/UPDATES/x.gz.partial"),
            b"half",
        )
        .unwrap();
        std::fs::write(
            dir.join("r2-adj-in-pre/bgpdata/2026.11/UPDATES/y.gz.tmp"),
            b"half",
        )
        .unwrap();

        let report = recover(&dir, "r2-adj-in-pre", "r2").unwrap();
        assert_eq!(report.closed_range.as_deref(), Some("segment"));
        assert_eq!(report.last_frame.as_deref(), Some("1763166170.000000"));
        assert_eq!(report.removed.len(), 2);
        // The claim is real and the file is not: this is the segment a restart must rebuild
        // under the identity the dead writer already published in the coverage log.
        assert_eq!(report.claims.len(), 1, "{:?}", report.claims);
        assert_eq!(
            report.claims[0],
            UnpublishedClaim {
                interval_start: 1_763_166_169_000_000,
                until: 1_763_166_170_000_001,
                session_epoch: 1,
                ordinal: 1,
            }
        );

        // The range is closed one microsecond after the last proven frame.
        let after = std::fs::read(&log).unwrap();
        let (timeline, summary) = Timeline::replay(&after);
        assert_eq!(summary.discarded_lines, 0);
        assert!(timeline.open_range().is_some());
        let closed = timeline.ranges();
        let segment = closed
            .iter()
            .find(|range| range.kind == RangeKind::Segment)
            .unwrap();
        assert_eq!(
            segment.end.unwrap().render(),
            "1763166170.000001",
            "the range ends where the record ends"
        );

        // Recovery is idempotent: nothing is open to close the second time.
        let again = recover(&dir, "r2-adj-in-pre", "r2").unwrap();
        assert!(again.closed_range.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_coverage_tail_is_cut_so_the_archive_can_open_again() {
        let dir = std::env::temp_dir().join(format!("bgpviews-torn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("coverage/r2-adj-in-pre")).unwrap();

        let good = encode_line(&Line::Open {
            at: stamp(1_763_166_169_000_000),
            range_kind: RangeKind::Gap,
            reason: Some("no_session".to_string()),
        });
        let log = dir.join("coverage/r2-adj-in-pre/updates.jsonl");
        let mut bytes = good.clone().into_bytes();
        bytes.extend_from_slice(b"{\"line\":\"transition\",\"at\":\"17631");
        std::fs::write(&log, &bytes).unwrap();

        // Before recovery the log is unusable, which is exactly what Collector::open refuses.
        let (_, before) = Timeline::replay(&std::fs::read(&log).unwrap());
        assert_eq!(before.discarded_lines, 1);

        let report = recover(&dir, "r2-adj-in-pre", "r2").unwrap();
        assert_eq!(report.repaired_lines, 1);
        assert_eq!(report.dropped_bytes, bytes.len() - good.len());
        assert!(report.problems.is_empty(), "{:?}", report.problems);

        let repaired = std::fs::read(&log).unwrap();
        assert_eq!(repaired, good.as_bytes(), "only the torn bytes are gone");
        let (_, after) = Timeline::replay(&repaired);
        assert_eq!(after.discarded_lines, 0);

        // And a second start finds nothing left to repair.
        let again = recover(&dir, "r2-adj-in-pre", "r2").unwrap();
        assert_eq!(again.repaired_lines, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_with_no_valid_line_is_reported_and_left_alone() {
        let dir = std::env::temp_dir().join(format!("bgpviews-hopeless-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("coverage/r2-adj-in-pre")).unwrap();
        let log = dir.join("coverage/r2-adj-in-pre/updates.jsonl");
        std::fs::write(&log, b"not a coverage line\n").unwrap();

        let report = recover(&dir, "r2-adj-in-pre", "r2").unwrap();
        assert_eq!(report.repaired_lines, 0);
        assert_eq!(report.problems.len(), 1);
        assert_eq!(std::fs::read(&log).unwrap(), b"not a coverage line\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
