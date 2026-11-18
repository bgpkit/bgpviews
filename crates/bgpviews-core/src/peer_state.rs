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
    /// Whether this version was written while the session was live, or rebuilt from the journal
    /// after the fact. A reader has to be able to tell an observation from a reconstruction, so
    /// the log says which it is instead of making the two indistinguishable.
    #[serde(default)]
    pub reconstructed: bool,
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
        // A log that already holds the magic but no frames is not a new log: appending a second
        // magic makes every later frame unreadable, because a reader sees the second magic as a
        // torn header and stops there. Only a file this call created gets the magic.
        let fresh = std::fs::metadata(&path).map_or(true, |meta| meta.len() == 0);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map(BufWriter::new)
            .map_err(|e| PeerStateError::Io {
                op: "open peer_state.log",
                source: e,
            })?;
        if fresh {
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

    /// Every object of one peer this log already knows.
    #[must_use]
    pub fn keys_for(&self, peer_key: &str) -> Vec<PeerObjectKey> {
        let mut found: Vec<PeerObjectKey> = self
            .last
            .keys()
            .filter(|key| key.peer_key == peer_key)
            .cloned()
            .collect();
        found.sort_by_key(|key| (key.afi, key.safi));
        found
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

/// Rebuild the versions a crash lost from the epoch's journal, and say how many were written.
///
/// Section 6 asks for exactly this: a version can reach the page cache and not the disk, so the
/// log can end without the last event the journal proves. The journal is the stronger record,
/// so every Peer Up, end-of-RIB and Peer Down it holds is re-derived here and appended when the
/// log's own last version for that object does not already say so.
///
/// A rebuilt version carries `reconstructed: true`. That flag is the whole point: a reader deciding
/// whether a segment's `complete` claim was honest has to know whether the collector saw this or
/// somebody worked it out afterwards.
///
/// # Errors
/// Returns [`PeerStateError`] when the journal or the log cannot be read, or the log cannot be
/// written.
pub fn rebuild_from_journal(
    root: &Path,
    client: &str,
    epoch: u64,
    view: &str,
) -> Result<usize, PeerStateError> {
    let dir = root
        .join("sessions")
        .join(client)
        .join(format!("{epoch:016}"));
    let mut parts: Vec<(u64, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
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
            parts.push((number, entry.path()));
        }
    }
    parts.sort();
    if parts.is_empty() {
        return Ok(0);
    }

    let mut writer = PeerStateWriter::open(root, client, epoch)?;
    // A peer's generation is a property of its Peer Up sequence, exactly as the live path counts
    // it: a second Peer Up for the same neighbour is generation 2, and its objects are new
    // objects. Writing every rebuilt version under generation 1 created keys the live path never
    // used and then applied later events to all of them.
    let mut generations: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut written = 0usize;
    for (_, part) in parts {
        let (frames, _summary) = journal::decode_frames(
            journal::MAGIC,
            &std::fs::read(&part).map_err(|e| PeerStateError::Io {
                op: "read a journal part",
                source: e,
            })?,
        )
        .map_err(PeerStateError::from)?;
        for frame in frames {
            if frame.kind != 0 {
                continue;
            }
            let at = frame
                .seconds
                .saturating_mul(1_000_000)
                .saturating_add(i64::from(frame.micros));
            let Ok(found) = crate::bmp::frames(&frame.payload) else {
                continue;
            };
            let Some(parsed) = found.into_iter().next() else {
                continue;
            };
            let (peer, state, eor, down) = match parsed.kind {
                crate::bmp::FrameKind::PeerUp {
                    peer_ip,
                    peer_asn,
                    families,
                    local_ip,
                    local_asn,
                    supported_peer,
                    loc_rib,
                    bgp_id: _,
                } => {
                    if !supported_peer {
                        continue;
                    }
                    // A Loc-RIB instance has no address and no OPEN of its own, so it cannot be
                    // keyed by `(peer_ip, peer_asn)` the way an adjacency is. Its state is left
                    // to the collector for now rather than written under a key that would
                    // collide with a global peer at 0.0.0.0.
                    if loc_rib.is_some() {
                        continue;
                    }
                    let peer_key = format!("{peer_ip}|{peer_asn}");
                    let generation = generations.get(&peer_key).copied().unwrap_or(0) + 1;
                    generations.insert(peer_key.clone(), generation);
                    let mut count = 0usize;
                    for family in families {
                        let key = PeerObjectKey {
                            client: client.to_string(),
                            session_epoch: epoch,
                            peer_key: peer_key.clone(),
                            peer_up_generation: generation,
                            view: view.to_string(),
                            afi: family.0,
                            safi: family.1 as u8,
                        };
                        if writer.last_payload(&key).is_some() {
                            continue;
                        }
                        writer.append(
                            key,
                            frame.seq,
                            PeerStatePayload {
                                key: PeerObjectKey {
                                    client: client.to_string(),
                                    session_epoch: epoch,
                                    peer_key: peer_key.clone(),
                                    peer_up_generation: generation,
                                    view: view.to_string(),
                                    afi: family.0,
                                    safi: family.1 as u8,
                                },
                                state_version: 0,
                                journal_seq: frame.seq,
                                observed_at: at,
                                add_path: None,
                                add_path_source: "unknown".to_string(),
                                local_as: local_asn,
                                local_ip: local_ip.to_string(),
                                peer_up_received_at: at,
                                dump_started_at: None,
                                eor_received_at: None,
                                eor_journal_seq: None,
                                peer_down_received_at: None,
                                peer_down_journal_seq: None,
                                last_received_at: at,
                                state: PeerState::Synced,
                                reconstructed: true,
                            },
                        )?;
                        count += 1;
                    }
                    written += count;
                    continue;
                }
                crate::bmp::FrameKind::RouteMonitoring {
                    peer_ip,
                    peer_asn,
                    is_eor,
                    eor_family,
                    ..
                } => {
                    let Some(family) = eor_family.filter(|_| is_eor) else {
                        continue;
                    };
                    (
                        format!("{peer_ip}|{peer_asn}"),
                        PeerState::Synced,
                        Some(family),
                        None,
                    )
                }
                crate::bmp::FrameKind::PeerDown {
                    peer_ip, peer_asn, ..
                } => (
                    format!("{peer_ip}|{peer_asn}"),
                    PeerState::Down,
                    None,
                    Some(()),
                ),
                _ => continue,
            };

            // One family per EoR and every declared family on a Peer Down, restricted to the
            // generation the journal's last Peer Up opened. Without a Peer Up there is no
            // generation to attribute the event to, so nothing is written: guessing would
            // stamp an event onto a generation that never saw it.
            let Some(generation) = generations.get(&peer).copied() else {
                continue;
            };
            let known: Vec<PeerObjectKey> = writer
                .keys_for(&peer)
                .into_iter()
                .filter(|key| key.peer_up_generation == generation)
                .filter(|key| match eor {
                    Some(family) => key.afi == family.0 && key.safi == family.1 as u8,
                    None => true,
                })
                .collect();
            for key in known {
                let last = writer.last_payload(&key).cloned();
                let unchanged = last.as_ref().is_some_and(|last| {
                    last.state == state
                        && last.eor_received_at.is_some() == eor.is_some()
                        && last.peer_down_received_at.is_some() == down.is_some()
                });
                if unchanged {
                    continue;
                }
                let base = last.unwrap_or(PeerStatePayload {
                    key: key.clone(),
                    state_version: 0,
                    journal_seq: frame.seq,
                    observed_at: at,
                    add_path: None,
                    add_path_source: "unknown".to_string(),
                    // The EoR and Peer Down frames carry no local address or AS; the object's own
                    // last version does, and this default is only reached for an object the log
                    // never saw, which the append above prevents.
                    local_as: 0,
                    local_ip: String::new(),
                    peer_up_received_at: at,
                    dump_started_at: None,
                    eor_received_at: None,
                    eor_journal_seq: None,
                    peer_down_received_at: None,
                    peer_down_journal_seq: None,
                    last_received_at: at,
                    state: PeerState::Synced,
                    reconstructed: true,
                });
                writer.append(
                    key,
                    frame.seq,
                    PeerStatePayload {
                        state,
                        eor_received_at: eor.map(|_| at).or(base.eor_received_at),
                        eor_journal_seq: eor.map(|_| frame.seq).or(base.eor_journal_seq),
                        peer_down_received_at: down.map(|()| at).or(base.peer_down_received_at),
                        peer_down_journal_seq: down
                            .map(|()| frame.seq)
                            .or(base.peer_down_journal_seq),
                        observed_at: at,
                        reconstructed: true,
                        ..base
                    },
                )?;
                written += 1;
            }
        }
    }
    Ok(written)
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
            reconstructed: false,
        }
    }

    #[test]
    fn a_version_the_crash_lost_is_rebuilt_from_the_journal_and_marked() {
        // The log is cut after the Peer Up version and the journal still holds the Peer Down.
        // Recovery has to append the missing state, and say that it was reconstructed.
        let root = temp("rebuild");
        let capture = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../lab/fixtures/frr-8.4-two-router.bin"),
        )
        .unwrap();
        let frames = crate::bmp::frames(&capture).unwrap();
        // Write the epoch's journal the way a dead session leaves it: every frame, no finish.
        let dir = root.join("sessions").join("edge1").join("0000000000000001");
        std::fs::create_dir_all(&dir).unwrap();
        let mut journal =
            crate::journal::JournalWriter::create(&dir.join("journal.0.bin")).unwrap();
        for (index, frame) in frames.iter().enumerate() {
            journal
                .append(1_794_807_600 + index as i64, 0, 0, &frame.bytes)
                .unwrap();
        }
        journal.sync().unwrap();
        drop(journal);

        // The log survived only up to its first version.
        let mut log = PeerStateWriter::open(&root, "edge1", 1).unwrap();
        log.append(
            key("adj-in-pre", 4),
            1,
            payload(1_794_807_600_000_000, PeerState::Synced),
        )
        .unwrap();
        drop(log);

        let written = rebuild_from_journal(&root, "edge1", 1, "adj-in-pre").unwrap();
        assert!(written > 0, "the rebuild appended something");

        let versions =
            replay(&root.join("sessions/edge1/0000000000000001/peer_state.log")).unwrap();
        assert_eq!(versions[0].1.state_version, 1);
        assert!(
            !versions[0].1.reconstructed,
            "the live version is an observation"
        );
        assert!(
            versions.iter().skip(1).all(|(_, p)| p.reconstructed),
            "every version after it is marked as rebuilt"
        );
        // A second rebuild finds nothing left to say.
        assert_eq!(
            rebuild_from_journal(&root, "edge1", 1, "adj-in-pre").unwrap(),
            0
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_log_holding_only_its_magic_is_not_given_a_second_one() {
        // A session that starts and writes no version leaves the magic and nothing else. The
        // next session to open this log must append after that magic, not write another one:
        // a reader stops at the second magic and every version after it is invisible.
        let root = temp("magic-only");
        drop(PeerStateWriter::open(&root, "edge1", 1).unwrap());
        let path = root.join("sessions/edge1/0000000000000001/peer_state.log");
        assert_eq!(std::fs::read(&path).unwrap(), MAGIC);

        let mut writer = PeerStateWriter::open(&root, "edge1", 1).unwrap();
        writer
            .append(
                key("adj-in-pre", 4),
                1,
                payload(1_794_807_600_000_000, PeerState::Synced),
            )
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            bytes.windows(MAGIC.len()).filter(|w| *w == MAGIC).count(),
            1,
            "one magic, not two"
        );
        let versions = replay(&path).unwrap();
        assert_eq!(versions.len(), 1, "the version after the magic is readable");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rebuild_attributes_each_event_to_the_generation_that_saw_it() {
        // Two Peer Ups for one neighbour are two generations. The rebuild must number the second
        // one 2 and must not stamp the Peer Down it carries onto generation 1's objects.
        let root = temp("generations");
        let capture = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../lab/fixtures/frr-8.4-two-router.bin"),
        )
        .unwrap();
        let frames = crate::bmp::frames(&capture).unwrap();
        let peer_ups: Vec<&crate::bmp::Frame> = frames
            .iter()
            .filter(|frame| matches!(frame.kind, crate::bmp::FrameKind::PeerUp { .. }))
            .collect();
        assert!(!peer_ups.is_empty(), "the fixture holds a Peer Up");

        // Build a journal from just the first Peer Up, the Peer Ups again, and a Peer Down, so
        // both generations appear in one epoch.
        let dir = root.join("sessions").join("edge1").join("0000000000000001");
        std::fs::create_dir_all(&dir).unwrap();
        let mut journal =
            crate::journal::JournalWriter::create(&dir.join("journal.0.bin")).unwrap();
        let mut seq = 0i64;
        let mut write = |bytes: &[u8], journal: &mut crate::journal::JournalWriter| {
            journal.append(1_794_807_600 + seq, 0, 0, bytes).unwrap();
            seq += 1;
        };
        // The same Peer Up twice: a repeat for the same neighbour is a new generation, which is
        // what a re-established session announces.
        let first_up = peer_ups[0];
        write(&first_up.bytes, &mut journal);
        write(&first_up.bytes, &mut journal);
        for frame in &frames {
            if matches!(frame.kind, crate::bmp::FrameKind::PeerDown { .. }) {
                write(&frame.bytes, &mut journal);
            }
        }
        journal.sync().unwrap();
        drop(journal);

        rebuild_from_journal(&root, "edge1", 1, "adj-in-pre").unwrap();
        let versions =
            replay(&root.join("sessions/edge1/0000000000000001/peer_state.log")).unwrap();
        let mut generations: Vec<u64> = versions
            .iter()
            .map(|(_, payload)| payload.key.peer_up_generation)
            .collect();
        generations.sort_unstable();
        generations.dedup();
        assert_eq!(generations, vec![1, 2], "both generations are represented");

        // The oldest generation keeps `synced`: the Peer Down belongs to generation 2.
        let first: Vec<_> = versions
            .iter()
            .filter(|(_, payload)| payload.key.peer_up_generation == 1)
            .collect();
        assert!(
            first
                .iter()
                .all(|(_, payload)| payload.state == PeerState::Synced),
            "generation 1 never sees generation 2's Peer Down: {first:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
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
