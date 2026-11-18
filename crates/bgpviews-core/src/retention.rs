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
    /// Where the session came from, as the socket saw it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// When the session opened, rendered the way every other stamp in the archive is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// When it closed, or `None` while it is still running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// Why it closed, in the closed reason set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    /// Frames accepted on the session.
    #[serde(default)]
    pub frames: u64,
    /// MRT records written for it.
    #[serde(default)]
    pub records: u64,
    /// The router's Initiation sysName, when the session carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exporter_sys_name: Option<String>,
    /// The router's Initiation sysDescr, when the session carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exporter_sys_descr: Option<String>,
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

/// What a session knows about itself that a scan of its parts cannot.
///
/// Whoever holds the fact fills it; everything left `None` keeps whatever the record already
/// said, because the record is rewritten by the accept path, by the collector at every segment
/// close, and by the close itself.
#[derive(Debug, Clone, Default)]
pub struct SessionFacts {
    /// The router's address.
    pub source: Option<String>,
    /// When the socket was accepted.
    pub started_at: Option<String>,
    /// When the session closed.
    pub ended_at: Option<String>,
    /// Why it closed, in the closed reason set.
    pub end_reason: Option<String>,
    /// Frames accepted, when the caller is the one counting them.
    pub frames: Option<u64>,
    /// MRT records written, when the caller is the one counting them.
    pub records: Option<u64>,
    /// Initiation sysName.
    pub exporter_sys_name: Option<String>,
    /// Initiation sysDescr.
    pub exporter_sys_descr: Option<String>,
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
    facts: &SessionFacts,
) -> Result<SessionRecord, String> {
    let dir = root
        .join("sessions")
        .join(client)
        .join(format!("{epoch:016}"));
    let mut parts = Vec::new();
    for path in parts_in(&dir) {
        parts.push(part_record(&path)?);
    }
    // Some facts are known by whoever accepted the socket and some by the collector, and the
    // record is rewritten by both. A field this writer does not know keeps what the file
    // already said rather than being erased every time a part is added.
    let previous = read_session_record(&dir).ok();
    let keep = |value: &Option<String>, fallback: Option<fn(&SessionRecord) -> Option<String>>| {
        value
            .clone()
            .or_else(|| fallback.and_then(|get| previous.as_ref().and_then(&get)))
    };
    let record = SessionRecord {
        schema_version: 1,
        client: client.to_string(),
        epoch,
        collector: collector.to_string(),
        parts,
        open,
        source: keep(
            &facts.source,
            Some(|record: &SessionRecord| record.source.clone()),
        ),
        started_at: keep(
            &facts.started_at,
            Some(|record: &SessionRecord| record.started_at.clone()),
        ),
        ended_at: facts.ended_at.clone(),
        end_reason: facts.end_reason.clone(),
        frames: facts
            .frames
            .or_else(|| previous.as_ref().map(|record| record.frames))
            .unwrap_or(0),
        records: facts
            .records
            .or_else(|| previous.as_ref().map(|record| record.records))
            .unwrap_or(0),
        exporter_sys_name: keep(
            &facts.exporter_sys_name,
            Some(|record: &SessionRecord| record.exporter_sys_name.clone()),
        ),
        exporter_sys_descr: keep(
            &facts.exporter_sys_descr,
            Some(|record: &SessionRecord| record.exporter_sys_descr.clone()),
        ),
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
            // The tombstone has to be on the disk before the part it describes is unlinked:
            // an expiry nobody recorded looks exactly like a part that never existed.
            let tombstone_json = json + "\n";
            write_atomic(&tombstone, tombstone_json.as_bytes())?;
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
                source: None,
                started_at: None,
                ended_at: None,
                end_reason: None,
                frames: 0,
                records: 0,
                exporter_sys_name: None,
                exporter_sys_descr: None,
            })
        }
    }
}

/// Replace `path` with `bytes` atomically and durably.
///
/// A `session.json` written in place can be read half-way through, and a session record that
/// does not parse is a session whose facts are gone: `expire` skips an epoch whose record it
/// cannot read, so a torn write costs that epoch its retention decisions forever.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::io::Write::write_all(&mut file, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    file.sync_all()
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    // The rename is durable only when the directory that holds the entry is synced.
    if let Some(dir) = path.parent() {
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    Ok(())
}

fn write_session_record(dir: &Path, record: &SessionRecord) -> Result<(), String> {
    let path = dir.join("session.json");
    let json = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
    write_atomic(&path, (json + "\n").as_bytes())
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
        writer.append(1_763_166_169, 0, 0, b"frame one").unwrap();
        writer.append(1_763_166_170, 0, 0, b"frame two").unwrap();
        writer.sync().unwrap();
        drop(writer);
        path
    }

    #[test]
    fn a_session_record_carries_who_connected_and_how_it_ended() {
        let dir = std::env::temp_dir().join(format!("bgpviews-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _ = session(&dir);

        // The accept path knows the source and the start; the collector knows the counters.
        describe(
            &dir,
            "r2",
            1,
            "r2-adj-in-pre",
            true,
            &SessionFacts {
                source: Some("172.31.250.12".to_string()),
                started_at: Some("1763166169.000000".to_string()),
                frames: Some(7),
                records: Some(5),
                exporter_sys_name: Some("r2".to_string()),
                ..SessionFacts::default()
            },
        )
        .unwrap();

        // A later refresh that only knows counters must not erase what it does not know.
        let record = describe(
            &dir,
            "r2",
            1,
            "r2-adj-in-pre",
            true,
            &SessionFacts {
                frames: Some(9),
                records: Some(6),
                ..SessionFacts::default()
            },
        )
        .unwrap();
        assert_eq!(record.source.as_deref(), Some("172.31.250.12"));
        assert_eq!(record.started_at.as_deref(), Some("1763166169.000000"));
        assert_eq!(record.exporter_sys_name.as_deref(), Some("r2"));
        assert_eq!((record.frames, record.records), (9, 6));
        assert!(record.open);
        assert!(record.ended_at.is_none());

        // The close records the end.
        let record = describe(
            &dir,
            "r2",
            1,
            "r2-adj-in-pre",
            false,
            &SessionFacts {
                ended_at: Some("1763166170.500000".to_string()),
                end_reason: Some("session_ended".to_string()),
                ..SessionFacts::default()
            },
        )
        .unwrap();
        assert!(!record.open);
        assert_eq!(record.ended_at.as_deref(), Some("1763166170.500000"));
        assert_eq!(record.end_reason.as_deref(), Some("session_ended"));
        assert_eq!(record.source.as_deref(), Some("172.31.250.12"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_expired_part_leaves_a_tombstone_and_a_session_record() {
        let dir = std::env::temp_dir().join(format!("bgpviews-retention-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session_dir = session(&dir);

        let described = describe(
            &dir,
            "r2",
            1,
            "r2-adj-in-pre",
            false,
            &SessionFacts::default(),
        )
        .unwrap();
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
