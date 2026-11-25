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

    // 1. Journal parts: find the last frame this archive can prove, and discard a torn tail.
    //
    // The bytes after the last complete frame were never acknowledged, so no reader may rely
    // on them, and leaving them in place made every later `verify` run report a finding that no
    // command could clear. They are kept beside the part as evidence. Only the newest part can
    // be torn: a part boundary is also a batch boundary, so a torn older part means frames were
    // lost in the middle and only an operator can decide what to do.
    let parts = journal_parts(root, client);
    let newest = parts.last().map(|(epoch, number, _)| (*epoch, *number));
    let mut last: Option<i64> = None;
    for (epoch, number, part) in &parts {
        match journal::replay(part) {
            Ok((frames, summary)) => {
                if summary.torn {
                    if Some((*epoch, *number)) != newest {
                        report.problems.push(format!(
                            "{}: a torn tail in a part that is not the newest; the frames after \
                             it are gone",
                            part.display()
                        ));
                    } else {
                        match discard_torn_tail(part, summary.valid_len) {
                            Ok(_tail) => report.torn_parts.push(part.clone()),
                            Err(e) => report.problems.push(format!("{}: {e}", part.display())),
                        }
                    }
                }
                if let Some(frame) = frames.last() {
                    let at = frame.seconds * 1_000_000 + i64::from(frame.micros);
                    last = Some(last.map_or(at, |current: i64| current.max(at)));
                }
            }
            Err(e) => {
                // A part the writer created and crashed before its magic reached the disk holds
                // nothing: removing it changes no record a reader could have seen, and leaving it
                // made `verify` report `corrupt` forever.
                if std::fs::metadata(part).is_ok_and(|meta| meta.len() == 0) {
                    match std::fs::remove_file(part) {
                        Ok(()) => report.removed.push(part.clone()),
                        Err(e) => report.problems.push(format!("{}: {e}", part.display())),
                    }
                } else {
                    report.problems.push(format!("{}: {e}", part.display()));
                }
            }
        }
    }
    if let Some(at) = last {
        report.last_frame = Some(crate::segment::render_stamp(at));
    }

    // 2. Temporary files: never part of the archive, so removing them changes nothing a
    // reader could have seen.
    // Only this collector's own tree: a second client's writer may be holding a temp file
    // right now, and deleting it would take a segment away from a live session. The archive
    // is shared; recovery is not.
    let mut stale = temporary_files(&root.join(collector));
    stale.extend(temporary_files(&root.join("quarantine").join(collector)));
    for path in stale {
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
    // A claim written by an earlier recovery and never repaired is still work: it is read back
    // here so that a second crash does not lose the repair the first one ordered.
    for claim in pending_claims(root, client) {
        if !report.claims.iter().any(|existing| {
            existing.session_epoch == claim.session_epoch && existing.ordinal == claim.ordinal
        }) {
            report.claims.push(claim);
        }
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
    // One microsecond after the later of the last proven frame and the instant the range opened:
    // the range covers what was written, and the successor starts where the record stops. A range
    // cannot close before it opened, and a segment opened at a boundary the writer observed with no
    // records after its last frame is exactly that case: the empty interval starts when the clock
    // rolled, which is later than the last frame the journal proves.
    let at = from.max(open.start.0) + 1;
    if let Some(segment) = open.segment {
        let claim = UnpublishedClaim {
            interval_start: open.start.0,
            until: at,
            session_epoch: segment.session_epoch,
            ordinal: segment.ordinal,
        };
        // The claim is written down before the gap that replaces it, so a process that dies
        // between the two leaves the work order on disk instead of throwing it away with its
        // memory. The next recovery reads it back and repairs the segment it names.
        write_claim(root, client, &claim).map_err(|e| format!("{}: {e}", log.display()))?;
        report.claims.push(claim);
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

/// Discard a torn journal tail and keep it beside the part as evidence.
///
/// The bytes after the last complete frame were never acknowledged. No reader may rely on
/// them, and leaving them in place makes `verify` report a torn part on every run while no
/// command in the archive is allowed to rewrite a journal.
fn discard_torn_tail(part: &Path, valid_len: usize) -> Result<PathBuf, String> {
    let bytes = std::fs::read(part).map_err(|e| format!("read a torn part: {e}"))?;
    if valid_len > bytes.len() {
        return Err(format!(
            "the replay claims {valid_len} valid bytes of {}",
            bytes.len()
        ));
    }
    let tail = PathBuf::from(format!("{}.torn", part.display()));
    std::fs::write(&tail, &bytes[valid_len..])
        .map_err(|e| format!("keep the torn tail at {}: {e}", tail.display()))?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(part)
        .map_err(|e| format!("open a torn part: {e}"))?;
    file.set_len(valid_len as u64)
        .map_err(|e| format!("truncate a torn part: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("sync a torn part: {e}"))?;
    Ok(tail)
}

/// Every journal part under `sessions/<client>/`, ordered by `(epoch, part number)`.
///
/// Both components are numeric: `journal.10.bin` sorts before `journal.2.bin` as a string, and a
/// later epoch's part 0 is newer than an earlier epoch's part 9. The last entry is the only one a
/// live writer can be holding, and the only one whose torn tail this recovery may discard.
fn journal_parts(root: &Path, client: &str) -> Vec<(u64, u64, PathBuf)> {
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
                continue;
            }
            // The grammar is `journal.<number>.bin`. A repaired tail is `journal.<n>.bin.torn`
            // and must not be replayed as a part, which is why this parses rather than testing
            // the prefix alone.
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(rest) = name.strip_prefix("journal.") else {
                continue;
            };
            let Some(number) = rest
                .strip_suffix(".bin")
                .and_then(|n| n.parse::<u64>().ok())
            else {
                continue;
            };
            let epoch = path
                .parent()
                .and_then(|dir| dir.file_name())
                .and_then(|name| name.to_str())
                .and_then(|name| name.parse::<u64>().ok())
                .unwrap_or(0);
            found.push((epoch, number, path));
        }
    }
    found.sort_by_key(|(epoch, number, _)| (*epoch, *number));
    found
}

/// The file that holds an un-repaired claim for one epoch.
#[must_use]
pub fn claim_path(root: &Path, client: &str, epoch: u64) -> std::path::PathBuf {
    root.join("sessions")
        .join(client)
        .join(format!("{epoch:016}"))
        .join("repair.json")
}

/// Write a claim where a later process can find it, before the coverage log stops claiming the
/// range it describes.
fn write_claim(root: &Path, client: &str, claim: &UnpublishedClaim) -> Result<(), std::io::Error> {
    let path = claim_path(root, client, claim.session_epoch);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = format!(
        "{{\"interval_start\":{},\"until\":{},\"session_epoch\":{},\"ordinal\":{}}}\n",
        claim.interval_start, claim.until, claim.session_epoch, claim.ordinal
    );
    let mut file = std::fs::File::create(&path)?;
    use std::io::Write;
    file.write_all(body.as_bytes())?;
    file.sync_all()
}

/// Read back the claims an earlier recovery wrote and never finished repairing.
#[must_use]
pub fn pending_claims(root: &Path, client: &str) -> Vec<UnpublishedClaim> {
    let mut found = Vec::new();
    let Ok(epochs) = std::fs::read_dir(root.join("sessions").join(client)) else {
        return found;
    };
    let mut paths: Vec<std::path::PathBuf> = epochs
        .flatten()
        .map(|entry| entry.path().join("repair.json"))
        .filter(|path| path.is_file())
        .collect();
    paths.sort();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let number = |name: &str| value.get(name).and_then(serde_json::Value::as_i64);
        let Some(interval_start) = number("interval_start") else {
            continue;
        };
        let Some(until) = number("until") else {
            continue;
        };
        let Some(session_epoch) = number("session_epoch") else {
            continue;
        };
        let Some(ordinal) = number("ordinal") else {
            continue;
        };
        found.push(UnpublishedClaim {
            interval_start,
            until,
            session_epoch: session_epoch as u64,
            ordinal: ordinal as u32,
        });
    }
    found
}

/// Forget a claim once its segment is published again.
///
/// # Errors
/// Returns the I/O error when the file exists and cannot be removed.
pub fn clear_claim(
    root: &Path,
    client: &str,
    claim: &UnpublishedClaim,
) -> Result<(), std::io::Error> {
    match std::fs::remove_file(claim_path(root, client, claim.session_epoch)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
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

    /// Part numbers are compared numerically, not as text: a session that rotated past part 9
    /// leaves `journal.10.bin` as the live part, and a string sort calls part 9 the newest, so the
    /// real torn tail is refused and `verify` stays red forever.
    #[test]
    fn a_torn_part_ten_is_the_newest_part() {
        let root = std::env::temp_dir().join(format!("bgpviews-torn-ten-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("sessions/edge1/0000000000000001");
        std::fs::create_dir_all(&dir).unwrap();
        for part in 0..=10u64 {
            let path = dir.join(format!("journal.{part}.bin"));
            let mut writer = crate::journal::JournalWriter::create(&path).unwrap();
            writer
                .append(1_794_807_600 + part as i64, 0, 1, &[1, 2, 3])
                .unwrap();
            writer.sync().unwrap();
        }
        let newest = dir.join("journal.10.bin");
        let mut bytes = std::fs::read(&newest).unwrap();
        bytes.extend_from_slice(&[0xAA]);
        std::fs::write(&newest, bytes).unwrap();

        let report = crate::recovery::recover(&root, "edge1-adj-in-pre", "edge1").unwrap();

        assert_eq!(
            report.torn_parts,
            vec![newest.clone()],
            "part 10 is the live part: {:?}",
            report.problems
        );
        assert!(!crate::journal::replay(&newest).unwrap().1.torn);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Parts are numbered per epoch, so a later epoch's part 0 is newer than an earlier epoch's
    /// part 3. Sorting by part number alone calls the wrong part the newest and leaves a real torn
    /// tail unrepaired while reporting it as lost.
    #[test]
    fn a_torn_part_in_the_newest_epoch_is_the_one_that_is_repaired() {
        let root = std::env::temp_dir().join(format!("bgpviews-torn-epoch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let first = root.join("sessions/edge1/0000000000000001");
        let second = root.join("sessions/edge1/0000000000000002");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        // The older epoch rotated to part 1 and is intact.
        let mut old = crate::journal::JournalWriter::create(&first.join("journal.1.bin")).unwrap();
        old.append(1_794_807_600, 0, 1, &[1, 2, 3]).unwrap();
        old.sync().unwrap();
        drop(old);
        // The newest epoch is part 0 and torn.
        let newest = second.join("journal.0.bin");
        let mut part = crate::journal::JournalWriter::create(&newest).unwrap();
        part.append(1_794_809_600, 0, 1, &[4, 5, 6]).unwrap();
        part.sync().unwrap();
        drop(part);
        let mut bytes = std::fs::read(&newest).unwrap();
        bytes.extend_from_slice(&[0xAA, 0xBB]);
        std::fs::write(&newest, bytes).unwrap();

        let report = crate::recovery::recover(&root, "edge1-adj-in-pre", "edge1").unwrap();

        assert_eq!(
            report.torn_parts,
            vec![newest.clone()],
            "the newest epoch's part is repaired, not the older one: {:?}",
            report.problems
        );
        assert!(!crate::journal::replay(&newest).unwrap().1.torn);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A writer killed mid-frame leaves bytes that were never acknowledged. They are discarded
    /// so `verify` stops reporting a torn part nothing can repair, and kept beside the part as
    /// the evidence that the tail existed.
    #[test]
    fn a_torn_part_is_truncated_and_its_tail_kept() {
        let root = std::env::temp_dir().join(format!("bgpviews-torn-part-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("sessions/edge1/0000000000000001");
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("journal.1.bin");
        let mut writer = crate::journal::JournalWriter::create(&part).unwrap();
        writer.append(1_794_809_600, 0, 1, &[1, 2, 3]).unwrap();
        writer.append(1_794_809_601, 0, 1, &[4, 5, 6]).unwrap();
        writer.sync().unwrap();
        drop(writer);
        let whole = std::fs::read(&part).unwrap();
        let mut torn = whole.clone();
        torn.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        std::fs::write(&part, &torn).unwrap();

        let report = crate::recovery::recover(&root, "edge1-adj-in-pre", "edge1").unwrap();

        assert_eq!(
            report.torn_parts,
            vec![part.clone()],
            "problems: {:?}",
            report.problems
        );
        assert_eq!(
            std::fs::read(&part).unwrap(),
            whole,
            "the surviving prefix is the record"
        );
        assert_eq!(
            std::fs::read(format!("{}.torn", part.display())).unwrap(),
            vec![0xAA, 0xBB, 0xCC],
            "the discarded bytes are kept as evidence"
        );
        let (frames, summary) = crate::journal::replay(&part).unwrap();
        assert_eq!(frames.len(), 2);
        assert!(!summary.torn, "a repaired part replays whole");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_claim_survives_a_second_crash_because_it_is_written_down() {
        let root = std::env::temp_dir().join(format!("bgpviews-claim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let claim = UnpublishedClaim {
            interval_start: 1_794_809_600_000_000,
            until: 1_794_809_660_000_000,
            session_epoch: 7,
            ordinal: 2,
        };
        // The first recovery ordered the repair and died before doing it; the coverage log now
        // holds the gap it wrote, so a second recovery has nothing open to close.
        write_claim(&root, "edge1", &claim).unwrap();
        let pending = pending_claims(&root, "edge1");
        assert_eq!(pending, vec![claim.clone()], "the work order is on disk");

        // Once the segment is published the record goes away, and nothing is left to retry.
        clear_claim(&root, "edge1", &claim).unwrap();
        assert!(pending_claims(&root, "edge1").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recovery_leaves_another_collectors_open_segment_alone() {
        let root =
            std::env::temp_dir().join(format!("bgpviews-recover-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // Two collectors on one archive, each mid-write.
        let mine = root.join("edge1-adj-in-pre/bgpdata/2026.11/UPDATES");
        let theirs = root.join("edge2-adj-in-pre/bgpdata/2026.11/UPDATES");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::create_dir_all(&theirs).unwrap();
        std::fs::write(mine.join("updates.20261118.0000.gz.tmp"), b"mine").unwrap();
        let live = theirs.join("updates.20261118.0000.gz.tmp");
        std::fs::write(&live, b"theirs").unwrap();

        crate::recovery::recover(&root, "edge1-adj-in-pre", "edge1").unwrap();

        assert!(
            !mine.join("updates.20261118.0000.gz.tmp").exists(),
            "mine is stale"
        );
        assert!(
            live.exists(),
            "a second client's writer still holds this file; deleting it would take a segment \
             away from a live session"
        );
        let _ = std::fs::remove_dir_all(&root);
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

    #[test]
    fn a_range_opened_after_the_last_frame_closes_forward() {
        // A tick-opened interval has no frame of its own: the writer rolled at the boundary and had
        // nothing to write. When that writer then dies, the last frame the journal proves is older
        // than the range it left open. Closing that range at the frame put a transition before the
        // range it follows, the checksummed timeline discarded the line, and every later session
        // refused to open the archive at all.
        let root =
            std::env::temp_dir().join(format!("bgpviews-recover-tick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let config = crate::collector::CollectorConfig {
            client: "edge1".to_string(),
            view: "adj-in-pre".to_string(),
            feed: "bmp".to_string(),
            epoch: 1,
            interval_seconds: 60,
            root: root.clone(),
            rib: false,
            rib_interval_seconds: 60,
            disk_warning_percent: 90,
            disk_critical_percent: 97,
            write_stall_ms: 2_000,
            backpressure_deadline_seconds: 300,
            journal_max_bytes: 0,
            journal_rotate_seconds: 0,
            journal_sync_bytes: 0,
            add_path: crate::collector::AddPathMode::Reject,
        };
        let start = 1_794_700_000_000_000i64;
        let mut collector = crate::collector::Collector::open_at(config, start).unwrap();
        let capture = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../lab/fixtures/frr-8.4-two-router.bin"),
        )
        .expect("the lab fixture is committed");
        let mut cursor = std::io::Cursor::new(capture);
        let mut clock = || start;
        crate::collector::pump(&mut cursor, &mut collector, &mut clock).unwrap();
        // Three intervals of silence later: the dump's interval is published and the interval the
        // clock is in is open, and no frame proves when that one started.
        collector
            .tick(start + 3 * 60 * 1_000_000)
            .expect("the boundary is closed");
        drop(collector);

        let report = recover(&root, "edge1-adj-in-pre", "edge1").expect("recovery runs");
        assert!(
            report.closed_range.is_some(),
            "the range the dead writer left open is closed: {report:?}"
        );

        let bytes = std::fs::read(root.join("coverage/edge1-adj-in-pre/updates.jsonl"))
            .expect("the coverage log");
        let (timeline, summary) = Timeline::replay(&bytes);
        assert_eq!(summary.discarded_lines, 0, "every line validates");
        assert!(
            timeline.check_invariants().is_ok(),
            "the timeline stays contiguous: {timeline:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
