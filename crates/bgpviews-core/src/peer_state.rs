//! The persisted per-peer state log, `sessions/<client>/<epoch>/peer_state.log`.
//!
//! Section 6 asks for a log, not a latest-value row: every version of every
//! `(peer_key, peer_up_generation, view, afi, safi)` object is appended, so the record of what
//! the collector believed at a given moment survives the process that believed it. The bytes are
//! the journal's framing with its own magic and canonical JSON payloads, and the frames are
//! written on Peer Up, Peer Down, EoR and at segment close, each followed by an fsync.
//!
//! The one rule a reader can rely on: `state_version` for an object never goes backwards, including
//! across a crash. A writer seeds its counters from the log it is appending to, so a restart
//! continues the numbering instead of repeating it.

use std::collections::HashMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::journal;

/// The magic section 6 fixes for this log, so it is never mistaken for a journal part.
pub const MAGIC: &[u8; 8] = b"BGPVPSL1";

/// The file name inside a session epoch directory.
pub const LOG_NAME: &str = "peer_state.log";

/// What the log is, according to the collector at the moment the version was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerState {
    /// The Peer Up was received and no end-of-RIB has arrived yet.
    Syncing,
    /// The Peer Up was received and the router is reporting that peer.
    Synced,
    /// A Peer Down was received for this generation.
    Down,
}

/// One object's identity. The key is in the payload as well as in the writer's map, because a log
/// line that cannot say what it describes is unreadable without the process that wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerObjectKey {
    /// The BGP speaker whose BMP session this is.
    pub client: String,
    /// The session epoch the object belongs to.
    pub session_epoch: u64,
    /// The peer's identity: type, distinguisher, address, AS, BGP ID, rendered as the collector
    /// renders it, because the type 3 key has a different shape and one string covers both.
    pub peer_key: String,
    /// Which Peer Up for that key this is, starting at 1.
    pub peer_up_generation: u64,
    /// The view the object feeds.
    pub view: String,
    /// Address family.
    pub afi: u16,
    /// Subsequent address family.
    pub safi: u8,
}

/// One complete version of one object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerStatePayload {
    /// The object this version describes.
    pub key: PeerObjectKey,
    /// Starts at 1 and increments on every replacement. Prior versions stay in the log.
    pub state_version: u64,
    /// The journal frame this version was written after, so a reader can line the two up.
    pub journal_seq: u64,
    /// Collector UTC when this version was written, microseconds since the epoch.
    pub observed_at: i64,
    /// Whether ADD-PATH was negotiated for this family, when the writer knew.
    pub add_path: Option<bool>,
    /// Where the ADD-PATH answer came from: `sent_open`, `received_open`, `none`, or `unknown`
    /// when this writer never consulted the OPENs.
    pub add_path_source: String,
    /// The configured local AS.
    pub local_as: u32,
    /// The monitored session's local address, as the Peer Up reported it.
    pub local_ip: String,
    /// When the Peer Up for this generation arrived.
    pub peer_up_received_at: i64,
    /// When the dump for this family started, if it has.
    pub dump_started_at: Option<i64>,
    /// The first end-of-RIB for this object, which is baseline evidence and not a state.
    pub eor_received_at: Option<i64>,
    /// The journal frame that EoR was read from.
    pub eor_journal_seq: Option<u64>,
    /// When the Peer Down for this generation arrived, if it has.
    pub peer_down_received_at: Option<i64>,
    /// The journal frame that Peer Down was read from.
    pub peer_down_journal_seq: Option<u64>,
    /// The last UPDATE this object received. Checkpointed at segment close, not per UPDATE.
    pub last_received_at: i64,
    /// The state, which is what the sidecar's `partial_reasons` are derived from.
    pub state: PeerState,
}

/// Why the log could not be written or read.
#[derive(Debug)]
pub enum PeerStateError {
    /// The file begins with a magic this reader does not know.
    BadMagic,
    /// A frame failed its checksum.
    Checksum {
        /// Offset of the frame.
        offset: usize,
        /// Sequence number read from the frame.
        seq: u64,
    },
    /// A payload is not the JSON this module writes.
    Payload {
        /// Sequence of the frame that held it.
        seq: u64,
        /// The decoder's complaint.
        detail: String,
    },
    /// A payload describes no object, which this module never writes.
    EmptyKey,
    /// An I/O error, with the operation that failed.
    Io {
        /// What was being attempted.
        op: &'static str,
        /// The underlying error.
        source: std::io::Error,
    },
}

impl fmt::Display for PeerStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => write!(f, "peer_state.log does not start with BGPVPSL1"),
            Self::Checksum { offset, seq } => {
                write!(f, "peer_state frame {seq} at {offset} fails its checksum")
            }
            Self::Payload { seq, detail } => {
                write!(
                    f,
                    "peer_state frame {seq} is not a payload this log can hold: {detail}"
                )
            }
            Self::EmptyKey => write!(f, "a payload names no object"),
            Self::Io { op, source } => write!(f, "{op}: {source}"),
        }
    }
}

impl std::error::Error for PeerStateError {}

impl From<journal::JournalError> for PeerStateError {
    fn from(error: journal::JournalError) -> Self {
        match error {
            journal::JournalError::BadMagic => Self::BadMagic,
            journal::JournalError::Checksum { offset, seq } => Self::Checksum { offset, seq },
            journal::JournalError::Torn { offset } => Self::Io {
                op: "read peer_state.log",
                source: std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("frame at {offset} is torn"),
                ),
            },
            journal::JournalError::SequenceGap { expected, found } => Self::Payload {
                seq: found,
                detail: format!("sequence {found} where {expected} was next"),
            },
            journal::JournalError::Io { op, source } => Self::Io { op, source },
        }
    }
}

/// Write one session epoch's `peer_state.log`.
pub struct PeerStateWriter {
    path: PathBuf,
    file: BufWriter<File>,
    next_seq: u64,
    versions: HashMap<PeerObjectKey, u64>,
    last: HashMap<PeerObjectKey, PeerStatePayload>,
}

impl PeerStateWriter {
    /// Open the log for an epoch, creating it and its directory if needed. An existing log is
    /// continued, never truncated: its frames are the record, and the version counters are seeded
    /// from it so a restart cannot reuse a `state_version`.
    ///
    /// # Errors
    /// Returns [`PeerStateError`] when the directory cannot be created, the log cannot be read or
    /// written, or the log it found is not one this module wrote.
    pub fn open(root: &Path, client: &str, epoch: u64) -> Result<Self, PeerStateError> {
        let dir = root
            .join("sessions")
            .join(client)
            .join(format!("{epoch:016}"));
        std::fs::create_dir_all(&dir).map_err(|e| PeerStateError::Io {
            op: "create the session directory",
            source: e,
        })?;
        let path = dir.join(LOG_NAME);
        let (next_seq, versions, last) =
            if path.is_file() {
                let (frames, summary) = read_with_summary(&path)?;
                if summary.torn {
                    // Appending behind a torn frame would leave bytes no reader can skip, so every
                    // version after the tear becomes unreadable. Section 6: keep the valid prefix and
                    // save the suffix beside it.
                    let bytes = std::fs::read(&path).map_err(|e| PeerStateError::Io {
                        op: "read peer_state.log",
                        source: e,
                    })?;
                    let kept = summary.valid_len.min(bytes.len());
                    std::fs::write(path.with_file_name("peer_state.torn"), &bytes[kept..])
                        .map_err(|e| PeerStateError::Io {
                            op: "save the torn peer_state suffix",
                            source: e,
                        })?;
                    let file = OpenOptions::new().write(true).open(&path).map_err(|e| {
                        PeerStateError::Io {
                            op: "open peer_state.log to cut its tail",
                            source: e,
                        }
                    })?;
                    file.set_len(kept as u64).map_err(|e| PeerStateError::Io {
                        op: "cut the torn peer_state tail",
                        source: e,
                    })?;
                }
                let mut versions: HashMap<PeerObjectKey, u64> = HashMap::new();
                let mut last: HashMap<PeerObjectKey, PeerStatePayload> = HashMap::new();
                for frame in &frames {
                    let (_seq, payload) = decode_payload(frame)?;
                    versions.insert(payload.key.clone(), payload.state_version);
                    last.insert(payload.key.clone(), payload);
                }
                let next_seq = frames.last().map_or(1, |frame| frame.seq + 1);
                (next_seq, versions, last)
            } else {
                (1, HashMap::new(), HashMap::new())
            };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map(BufWriter::new)
            .map_err(|e| PeerStateError::Io {
                op: "open peer_state.log",
                source: e,
            })?;
        if next_seq == 1 {
            file.write_all(MAGIC)
                .and_then(|()| file.flush())
                .map_err(|e| PeerStateError::Io {
                    op: "write the peer_state magic",
                    source: e,
                })?;
        }
        Ok(Self {
            path,
            file,
            next_seq,
            versions,
            last,
        })
    }

    /// The log this writer appends to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one version and make it durable. The caller fills the payload's timestamps and
    /// state; this method owns `state_version` and `journal_seq`, because they are properties of
    /// the log and not of the caller's view.
    ///
    /// # Errors
    /// Returns [`PeerStateError`] when the frame cannot be written or synced.
    pub fn append(
        &mut self,
        key: PeerObjectKey,
        journal_seq: u64,
        mut payload: PeerStatePayload,
    ) -> Result<u64, PeerStateError> {
        if key.peer_key.is_empty() || key.client.is_empty() {
            return Err(PeerStateError::EmptyKey);
        }
        let version = self.versions.get(&key).copied().unwrap_or(0) + 1;
        payload.key = key.clone();
        payload.state_version = version;
        payload.journal_seq = journal_seq;
        let body = serde_json::to_vec(&payload).map_err(|e| PeerStateError::Payload {
            seq: 0,
            detail: e.to_string(),
        })?;
        let seq = self.next_seq;
        let frame = journal::encode_frame(
            seq,
            payload.observed_at.div_euclid(1_000_000),
            payload.observed_at.rem_euclid(1_000_000) as u32,
            0,
            &body,
        );
        self.file
            .write_all(&frame)
            .and_then(|()| self.file.flush())
            .map_err(|e| PeerStateError::Io {
                op: "write a peer_state frame",
                source: e,
            })?;
        // Peer state is the evidence a later reader uses to decide whether a segment's `complete`
        // claim was honest, so a version that reached the page cache and not the disk is not
        // worth writing.
        self.file
            .get_ref()
            .sync_data()
            .map_err(|e| PeerStateError::Io {
                op: "sync peer_state.log",
                source: e,
            })?;
        self.next_seq += 1;
        self.versions.insert(key.clone(), version);
        self.last.insert(key, payload);
        Ok(seq)
    }

    /// The last payload written for an object, which is what "did anything change" is measured
    /// against: the log itself, not this process's memory of it.
    #[must_use]
    pub fn last_payload(&self, key: &PeerObjectKey) -> Option<&PeerStatePayload> {
        self.last.get(key)
    }

    /// The last version written for an object, if any.
    #[must_use]
    pub fn version_of(&self, key: &PeerObjectKey) -> Option<u64> {
        self.versions.get(key).copied()
    }
}

/// Read every frame of a log, in order.
///
/// # Errors
/// Returns [`PeerStateError`] when the file cannot be read, its magic is not this log's, or a
/// frame fails its checksum.
pub fn read(path: &Path) -> Result<Vec<journal::JournaledFrame>, PeerStateError> {
    Ok(read_with_summary(path)?.0)
}

/// Read the frames and the summary, which is how a caller learns that the tail was cut short.
pub fn read_with_summary(
    path: &Path,
) -> Result<(Vec<journal::JournaledFrame>, journal::JournalSummary), PeerStateError> {
    let bytes = std::fs::read(path).map_err(|e| PeerStateError::Io {
        op: "read peer_state.log",
        source: e,
    })?;
    let (frames, summary) = journal::decode_frames(MAGIC, &bytes)?;
    Ok((frames, summary))
}

/// Read every payload of a log, in order, with the sequence each was written under.
///
/// # Errors
/// Returns [`PeerStateError`] when the log cannot be read or a payload cannot be decoded.
pub fn replay(path: &Path) -> Result<Vec<(u64, PeerStatePayload)>, PeerStateError> {
    let frames = read(path)?;
    frames.iter().map(decode_payload).collect()
}

fn decode_payload(
    frame: &journal::JournaledFrame,
) -> Result<(u64, PeerStatePayload), PeerStateError> {
    let payload: PeerStatePayload =
        serde_json::from_slice(&frame.payload).map_err(|e| PeerStateError::Payload {
            seq: frame.seq,
            detail: e.to_string(),
        })?;
    Ok((frame.seq, payload))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("bgpviews-peer-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn key(view: &str, afi: u16) -> PeerObjectKey {
        PeerObjectKey {
            client: "edge1".to_string(),
            session_epoch: 1,
            peer_key: "0|0|192.0.2.1|64500|192.0.2.1".to_string(),
            peer_up_generation: 1,
            view: view.to_string(),
            afi,
            safi: 1,
        }
    }

    fn payload(at: i64, state: PeerState) -> PeerStatePayload {
        PeerStatePayload {
            key: key("adj-in-pre", 4),
            state_version: 0,
            journal_seq: 0,
            observed_at: at,
            add_path: Some(false),
            add_path_source: "none".to_string(),
            local_as: 64512,
            local_ip: "192.0.2.254".to_string(),
            peer_up_received_at: at,
            dump_started_at: None,
            eor_received_at: None,
            eor_journal_seq: None,
            peer_down_received_at: None,
            peer_down_journal_seq: None,
            last_received_at: at,
            state,
        }
    }

    #[test]
    fn a_restart_continues_the_versions_instead_of_repeating_them() {
        let root = temp("versions");
        let mut writer = PeerStateWriter::open(&root, "edge1", 1).unwrap();
        writer
            .append(
                key("adj-in-pre", 4),
                10,
                payload(1_794_807_600_000_000, PeerState::Syncing),
            )
            .unwrap();
        writer
            .append(
                key("adj-in-pre", 4),
                11,
                payload(1_794_807_601_000_000, PeerState::Synced),
            )
            .unwrap();
        drop(writer);

        // A crash and restart: the counters come from the log, not from memory.
        let mut writer = PeerStateWriter::open(&root, "edge1", 1).unwrap();
        assert_eq!(writer.version_of(&key("adj-in-pre", 4)), Some(2));
        writer
            .append(
                key("adj-in-pre", 4),
                12,
                payload(1_794_807_602_000_000, PeerState::Down),
            )
            .unwrap();

        let versions: Vec<u64> = replay(writer.path())
            .unwrap()
            .iter()
            .map(|(_, p)| p.state_version)
            .collect();
        assert_eq!(versions, vec![1, 2, 3]);
    }

    #[test]
    fn an_object_that_was_never_written_starts_at_one() {
        let root = temp("fresh");
        let mut writer = PeerStateWriter::open(&root, "edge1", 1).unwrap();
        let seq = writer
            .append(
                key("adj-in-post", 6),
                5,
                payload(1_794_807_600_000_000, PeerState::Syncing),
            )
            .unwrap();
        assert_eq!(seq, 1);

        let versions = replay(writer.path()).unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].1.state_version, 1);
        assert_eq!(
            versions[0].1.key.afi, 6,
            "the payload says what it describes"
        );
        assert!(matches!(versions[0].1.state, PeerState::Syncing));
    }

    #[test]
    fn the_bytes_are_the_logs_magic_and_a_payload_round_trips() {
        let root = temp("round");
        let mut writer = PeerStateWriter::open(&root, "edge1", 1).unwrap();
        let mut written = payload(1_794_807_600_123_456, PeerState::Synced);
        written.eor_received_at = Some(1_794_807_700_000_000);
        written.eor_journal_seq = Some(99);
        writer
            .append(key("adj-in-pre", 4), 42, written.clone())
            .unwrap();
        let bytes = std::fs::read(writer.path()).unwrap();
        assert_eq!(&bytes[..8], b"BGPVPSL1");

        let replayed = replay(writer.path()).unwrap();
        assert_eq!(replayed.len(), 1);
        // The writer owns the version and the journal sequence; everything else is the caller's.
        let mut expected = written.clone();
        expected.state_version = 1;
        expected.journal_seq = 42;
        assert_eq!(replayed[0].1, expected);
    }

    #[test]
    fn a_torn_tail_keeps_the_versions_before_it() {
        let root = temp("torn");
        let mut writer = PeerStateWriter::open(&root, "edge1", 1).unwrap();
        writer
            .append(
                key("adj-in-pre", 4),
                1,
                payload(1_794_807_600_000_000, PeerState::Syncing),
            )
            .unwrap();
        writer
            .append(
                key("adj-in-pre", 4),
                2,
                payload(1_794_807_601_000_000, PeerState::Synced),
            )
            .unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);

        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 4);
        std::fs::write(&path, &bytes).unwrap();

        // A reader keeps the valid prefix and reports the tear.
        let (frames, summary) = read_with_summary(&path).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(summary.torn);

        // A writer cuts it, keeps the suffix, and appends after the last whole frame. Without
        // this the next append lands behind garbage and every later version is unreadable.
        let mut writer = PeerStateWriter::open(&root, "edge1", 1).unwrap();
        assert_eq!(writer.version_of(&key("adj-in-pre", 4)), Some(1));
        writer
            .append(
                key("adj-in-pre", 4),
                3,
                payload(1_794_807_602_000_000, PeerState::Down),
            )
            .unwrap();

        let replayed = replay(writer.path()).unwrap();
        assert_eq!(replayed.len(), 2, "the whole version and the new one");
        assert_eq!(replayed[0].1.state_version, 1);
        assert_eq!(replayed[1].1.state_version, 2);
        assert_eq!(replayed[1].1.state, PeerState::Down);

        let torn =
            std::fs::read(root.join("sessions/edge1/0000000000000001/peer_state.torn")).unwrap();
        assert!(
            !torn.is_empty(),
            "the dropped suffix is kept beside the log"
        );
    }
}
