//! Retention: dropping journal parts without losing the record that they existed.
//!
//! Design reference: `docs/design.md` section 11. The journal is what makes an archive
//! auditable, and it is also the largest thing on disk. Deleting a part outright would mean
//! the catalog could still name a session whose raw frames are gone with no way to tell
//! "never written" from "expired".
//!
//! So expiry is per part, and each expired part leaves a tombstone: name, size, sha256,
//! sequence range, and when it went. `session.json` keeps the list and marks the expiry, so a
//! reader can see that a session had twelve parts and that nine of them were dropped by
//! retention rather than lost.
//!
//! A part is expired only when its last frame is older than the cutoff. Mtime is not used:
//! a copy or a restore rewrites mtimes, and the frames carry their own times.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::journal;

/// One part of a session's journal, as `session.json` records it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PartRecord {
    /// File name, `journal.N.bin`.
    pub name: String,
    /// Bytes on disk when the record was written.
    pub bytes: u64,
    /// SHA-256 of those bytes.
    pub sha256: String,
    /// First sequence number, starting at 1.
    pub first_seq: u64,
    /// Last sequence number.
    pub last_seq: u64,
    /// Last frame time, rendered, when the part had frames.
    pub last_frame: Option<String>,
    /// When the part was dropped, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expired_at: Option<i64>,
}

/// What `session.json` holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRecord {
    /// Always 1.
    pub schema_version: u32,
    /// The configured client name.
    pub client: String,
    /// The session epoch.
    pub epoch: u64,
    /// The collector this session belongs to.
    pub collector: String,
    /// Parts, in order.
    pub parts: Vec<PartRecord>,
    /// Whether the session that owns these parts is still running.
    ///
    /// An open session's newest part is the one its writer still holds, so retention leaves it
    /// alone; a closed session's parts are all fair game. A record written before this field
    /// existed reads as open, which keeps an unknown session's bytes rather than guessing.
    #[serde(default = "default_open")]
    pub open: bool,
}

fn default_open() -> bool {
    true
}

/// What an expiry run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Parts expired, by path.
    pub expired: Vec<PathBuf>,
    /// Tombstones written.
    pub tombstones: Vec<PathBuf>,
    /// Parts kept because they are newer than the cutoff.
    pub kept: usize,
    /// Problems that did not stop the run.
    pub problems: Vec<String>,
}

/// Describe a session's journal parts, writing `session.json`.
///
/// # Errors
/// Returns a message when the session directory cannot be read or the record cannot be
/// written.
pub fn describe(
    root: &Path,
    client: &str,
    epoch: u64,
    collector: &str,
    open: bool,
) -> Result<SessionRecord, String> {
    let dir = root
        .join("sessions")
        .join(client)
        .join(format!("{epoch:016}"));
    let mut parts = Vec::new();
    for path in parts_in(&dir) {
        parts.push(part_record(&path)?);
    }
    let record = SessionRecord {
        schema_version: 1,
        client: client.to_string(),
        epoch,
        collector: collector.to_string(),
        parts,
        open,
    };
    write_session_record(&dir, &record)?;
    Ok(record)
}

/// Drop journal parts whose last frame is older than `cutoff`.
///
/// `cutoff` is seconds since the epoch. `keep_forever` short-circuits the whole thing.
///
/// # Errors
/// Returns a message when a session record cannot be read or written, or a tombstone cannot
/// be created. A part is only removed after its tombstone is durable.
pub fn expire(root: &Path, client: &str, cutoff: i64) -> Result<Report, String> {
    let mut report = Report::default();
    let sessions = root.join("sessions").join(client);
    let Ok(entries) = std::fs::read_dir(&sessions) else {
        return Ok(report);
    };
    let mut epoch_dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    epoch_dirs.sort();

    for dir in epoch_dirs {
        let mut record = match read_session_record(&dir) {
            Ok(record) => record,
            Err(e) => {
                report.problems.push(e);
                continue;
            }
        };
        let last_part = record.parts.len().saturating_sub(1);
        for (index, part) in record.parts.iter_mut().enumerate() {
            if part.expired_at.is_some() {
                continue;
            }
            // The last part is the one a live session is writing: its last frame is only as
            // new as the last segment close, and expiring it would truncate the journal the
            // writer still holds. It becomes expirable when a newer part exists.
            if record.open && index == last_part {
                report.kept += 1;
                continue;
            }
            let path = dir.join(&part.name);
            let older = part
                .last_frame
                .as_deref()
                .and_then(|stamp| stamp.split('.').next())
                .and_then(|seconds| seconds.parse::<i64>().ok())
                .is_some_and(|seconds| seconds < cutoff);
            if !older {
                report.kept += 1;
                continue;
            }
            // Tombstone first: the record of the expiry must be durable before the bytes
            // it describes disappear.
            let tombstone = dir.join(format!("{}.tombstone.json", part.name));
            let json = serde_json::to_string_pretty(part).map_err(|e| e.to_string())?;
            std::fs::write(&tombstone, json + "\n")
                .map_err(|e| format!("{}: {e}", tombstone.display()))?;
            report.tombstones.push(tombstone);
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    report.expired.push(path);
                    part.expired_at = Some(cutoff);
                }
                Err(e) => report.problems.push(format!("{}: {e}", path.display())),
            }
        }
        write_session_record(&dir, &record)?;
    }
    Ok(report)
}

/// Read `session.json` for a session, or describe it if the record is missing.
fn read_session_record(dir: &Path) -> Result<SessionRecord, String> {
    let path = dir.join("session.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(_) => {
            // A session from before this record existed: describe what is on disk.
            let client = dir
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let epoch = dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0);
            let mut parts = Vec::new();
            for part in parts_in(dir) {
                parts.push(part_record(&part)?);
            }
            Ok(SessionRecord {
                schema_version: 1,
                client,
                epoch,
                collector: String::new(),
                parts,
                open: true,
            })
        }
    }
}

fn write_session_record(dir: &Path, record: &SessionRecord) -> Result<(), String> {
    let path = dir.join("session.json");
    let json = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
    std::fs::write(&path, json + "\n").map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

fn part_record(path: &Path) -> Result<PartRecord, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let sha256 = crate::segment::sha256_hex(&bytes);
    let (frames, _summary) =
        journal::replay_bytes(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    let first_seq = frames.first().map_or(0, |frame| frame.seq);
    let last_seq = frames.last().map_or(0, |frame| frame.seq);
    let last_frame = frames.last().map(|frame| {
        crate::segment::render_stamp(frame.seconds * 1_000_000 + i64::from(frame.micros))
    });
    Ok(PartRecord {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        bytes: bytes.len() as u64,
        sha256,
        first_seq,
        last_seq,
        last_frame,
        expired_at: None,
    })
}

fn parts_in(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .map(|n| {
                            let name = n.to_string_lossy();
                            name.starts_with("journal.") && name.ends_with(".bin")
                        })
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn session(dir: &Path) -> PathBuf {
        let path = dir.join("sessions/r2/0000000000000001");
        std::fs::create_dir_all(&path).unwrap();
        let mut writer = journal::JournalWriter::create(&path.join("journal.0.bin")).unwrap();
        writer.append(1_763_166_169, 0, b"frame one").unwrap();
        writer.append(1_763_166_170, 0, b"frame two").unwrap();
        writer.sync().unwrap();
        drop(writer);
        path
    }

    #[test]
    fn an_expired_part_leaves_a_tombstone_and_a_session_record() {
        let dir = std::env::temp_dir().join(format!("bgpviews-retention-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session_dir = session(&dir);

        let described = describe(&dir, "r2", 1, "r2-adj-in-pre", false).unwrap();
        assert_eq!(described.parts.len(), 1);
        assert_eq!(described.parts[0].first_seq, 1);
        assert_eq!(described.parts[0].last_seq, 2);
        assert!(session_dir.join("session.json").exists());

        // A cutoff after the last frame expires the part.
        let report = expire(&dir, "r2", 1_763_166_200).unwrap();
        assert_eq!(report.expired.len(), 1);
        assert_eq!(report.kept, 0);
        assert!(!session_dir.join("journal.0.bin").exists());
        assert!(session_dir.join("journal.0.bin.tombstone.json").exists());

        let record = read_session_record(&session_dir).unwrap();
        assert!(
            record.parts[0].expired_at.is_some(),
            "the record says it expired"
        );
        assert_eq!(record.parts[0].last_seq, 2, "the range survives the bytes");

        // A cutoff before the frames keeps whatever is left, and the run is idempotent.
        let again = expire(&dir, "r2", 1_763_166_000).unwrap();
        assert!(again.expired.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
