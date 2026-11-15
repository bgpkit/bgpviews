//! The session journal: every accepted frame, in order, with the bytes intact.
//!
//! Design reference: `docs/design.md` section 6. The journal is the record of
//! what arrived, and every other artifact is derived from it. That ordering is
//! the reason a parser bug cannot lose data: the segment writer reads frames out
//! of the journal, so a frame the parser mishandles is still on disk and can be
//! re-derived after the parser is fixed.
//!
//! Frame layout, one after another, big-endian:
//!
//! ```text
//! magic  8   "BGPVPJ01"
//! seq    8   monotone, starts at 1
//! secs   8   receive time, seconds since the epoch
//! micros 4   receive time, microseconds
//! len    4   payload length
//! crc    4   CRC-32C over seq..len and the payload
//! payload len  the frame, exactly as received
//! ```
//!
//! The CRC covers the header fields as well as the payload, so a torn write is
//! detectable in the header and not only in the body. Reading stops at the first
//! frame that does not validate, and the caller is told how many bytes survived.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// The part file's first bytes. A reader that does not see these refuses the file
/// rather than guessing at an older layout.
pub const MAGIC: &[u8; 8] = b"BGPVPJ01";

const HEADER_LEN: usize = 8 + 8 + 8 + 4 + 4 + 4;

/// One journaled frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournaledFrame {
    /// Position in the session, starting at 1.
    pub seq: u64,
    /// Receive time, seconds.
    pub seconds: i64,
    /// Receive time, microseconds.
    pub micros: u32,
    /// The frame's bytes.
    pub payload: Vec<u8>,
}

/// Why a journal could not be written or read.
#[derive(Debug)]
pub enum JournalError {
    /// The file does not begin with the expected magic.
    BadMagic,
    /// A frame header or body was cut short.
    Torn {
        /// Offset of the torn frame.
        offset: usize,
    },
    /// A frame's checksum did not match its contents.
    Checksum {
        /// Offset of the frame.
        offset: usize,
        /// Sequence number read from the frame.
        seq: u64,
    },
    /// The sequence numbers are not contiguous from 1.
    SequenceGap {
        /// The sequence expected next.
        expected: u64,
        /// The sequence found.
        found: u64,
    },
    /// An I/O error, with the operation that failed.
    Io {
        /// What was being attempted.
        op: &'static str,
        /// The underlying error.
        source: std::io::Error,
    },
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic => write!(f, "journal does not start with BGPVPJ01"),
            Self::Torn { offset } => write!(f, "journal ends inside a frame at offset {offset}"),
            Self::Checksum { offset, seq } => {
                write!(
                    f,
                    "journal frame {seq} at offset {offset} fails its checksum"
                )
            }
            Self::SequenceGap { expected, found } => {
                write!(f, "journal expected sequence {expected}, found {found}")
            }
            Self::Io { op, source } => write!(f, "journal {op} failed: {source}"),
        }
    }
}

impl std::error::Error for JournalError {}

/// What a replay found at the end of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalSummary {
    /// Bytes that belong to complete, valid frames.
    pub valid_len: usize,
    /// Frames read.
    pub frames: u64,
    /// Whether the file ended inside a frame.
    pub torn: bool,
}

/// A journal part being written.
pub struct JournalWriter {
    file: BufWriter<File>,
    path: PathBuf,
    next_seq: u64,
    written: u64,
}

impl JournalWriter {
    /// Create a new part. The magic is written and flushed immediately, so an
    /// empty part is still identifiable.
    ///
    /// # Errors
    /// Returns [`JournalError::Io`] if the directory or file cannot be created.
    pub fn create(path: &Path) -> Result<Self, JournalError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| JournalError::Io {
                op: "create directory",
                source: e,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .map_err(|e| JournalError::Io {
                op: "open part",
                source: e,
            })?;
        let mut writer = Self {
            file: BufWriter::new(file),
            path: path.to_path_buf(),
            next_seq: 1,
            written: 0,
        };
        writer.file.write_all(MAGIC).map_err(|e| JournalError::Io {
            op: "write magic",
            source: e,
        })?;
        Ok(writer)
    }

    /// Append one frame and fsync. The caller batches frames outside this type;
    /// this method's contract is that a returned `Ok` means the frame is durable.
    ///
    /// # Errors
    /// Returns [`JournalError::Io`] when the write or the fsync fails.
    pub fn append(
        &mut self,
        seconds: i64,
        micros: u32,
        payload: &[u8],
    ) -> Result<u64, JournalError> {
        let seq = self.next_seq;
        let len = u32::try_from(payload.len()).map_err(|_| JournalError::Io {
            op: "encode length",
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame too large"),
        })?;

        let mut header = Vec::with_capacity(HEADER_LEN - 8);
        header.extend_from_slice(&seq.to_be_bytes());
        header.extend_from_slice(&seconds.to_be_bytes());
        header.extend_from_slice(&micros.to_be_bytes());
        header.extend_from_slice(&len.to_be_bytes());

        let mut crc = crate::crc::Crc32c::new();
        crc.update(&header);
        crc.update(payload);
        let checksum = crc.finish();

        let write = |file: &mut BufWriter<File>| -> std::io::Result<()> {
            file.write_all(&header)?;
            file.write_all(&checksum.to_be_bytes())?;
            file.write_all(payload)?;
            Ok(())
        };
        write(&mut self.file).map_err(|e| JournalError::Io {
            op: "write frame",
            source: e,
        })?;
        self.file.flush().map_err(|e| JournalError::Io {
            op: "flush",
            source: e,
        })?;
        self.file
            .get_ref()
            .sync_data()
            .map_err(|e| JournalError::Io {
                op: "fsync",
                source: e,
            })?;

        self.next_seq += 1;
        self.written += 1;
        Ok(seq)
    }

    /// Frames written to this part.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.written
    }

    /// The part's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Replay a journal part into frames.
///
/// # Errors
/// Returns [`JournalError::BadMagic`] when the file is not a journal, and
/// [`JournalError::Io`] when it cannot be read. A torn tail is not an error: it is
/// reported in the summary, because a crash mid-append is the ordinary case.
pub fn replay(path: &Path) -> Result<(Vec<JournaledFrame>, JournalSummary), JournalError> {
    let bytes = std::fs::read(path).map_err(|e| JournalError::Io {
        op: "read part",
        source: e,
    })?;
    replay_bytes(&bytes)
}

/// Replay a journal part from memory.
///
/// # Errors
/// As [`replay`].
pub fn replay_bytes(bytes: &[u8]) -> Result<(Vec<JournaledFrame>, JournalSummary), JournalError> {
    if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
        return Err(JournalError::BadMagic);
    }
    let mut frames = Vec::new();
    let mut offset = MAGIC.len();
    let mut valid_len = offset;
    let mut torn = false;

    while offset < bytes.len() {
        if bytes.len() - offset < HEADER_LEN - MAGIC.len() {
            torn = true;
            break;
        }
        let rest = &bytes[offset..];
        let seq = u64::from_be_bytes(rest[0..8].try_into().unwrap_or_default());
        let seconds = i64::from_be_bytes(rest[8..16].try_into().unwrap_or_default());
        let micros = u32::from_be_bytes(rest[16..20].try_into().unwrap_or_default());
        let len = u32::from_be_bytes(rest[20..24].try_into().unwrap_or_default()) as usize;
        let checksum = u32::from_be_bytes(rest[24..28].try_into().unwrap_or_default());
        let body_start = offset + (HEADER_LEN - MAGIC.len());
        if rest.len() < (HEADER_LEN - MAGIC.len()) + len {
            torn = true;
            break;
        }
        let payload = &bytes[body_start..body_start + len];

        let mut crc = crate::crc::Crc32c::new();
        crc.update(&rest[0..24]);
        crc.update(payload);
        if crc.finish() != checksum {
            return Err(JournalError::Checksum { offset, seq });
        }
        let expected = frames.len() as u64 + 1;
        if seq != expected {
            return Err(JournalError::SequenceGap {
                expected,
                found: seq,
            });
        }
        frames.push(JournaledFrame {
            seq,
            seconds,
            micros,
            payload: payload.to_vec(),
        });
        offset = body_start + len;
        valid_len = offset;
    }

    Ok((
        frames,
        JournalSummary {
            valid_len,
            frames: 0,
            torn,
        },
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("bgpviews-journal-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn frames_round_trip_with_their_timestamps() {
        let path = temp("round-trip.bin");
        let mut writer = JournalWriter::create(&path).unwrap();
        let first = writer.append(1_800_000_000, 1, b"first").unwrap();
        let second = writer.append(1_800_000_001, 2, b"second").unwrap();
        assert_eq!((first, second), (1, 2));
        drop(writer);

        let (frames, summary) = replay(&path).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].payload, b"first");
        assert_eq!(frames[1].seconds, 1_800_000_001);
        assert_eq!(frames[1].micros, 2);
        assert!(!summary.torn);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_torn_tail_keeps_the_frames_before_it() {
        let path = temp("torn.bin");
        let mut writer = JournalWriter::create(&path).unwrap();
        writer.append(1_800_000_000, 0, b"kept").unwrap();
        writer.append(1_800_000_001, 0, b"cut-off").unwrap();
        drop(writer);

        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 3);
        let (frames, summary) = replay_bytes(&bytes).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload, b"kept");
        assert!(summary.torn);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_flipped_payload_byte_is_a_checksum_error_not_a_silent_record() {
        let path = temp("flip.bin");
        let mut writer = JournalWriter::create(&path).unwrap();
        writer.append(1_800_000_000, 0, b"payload").unwrap();
        drop(writer);

        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let err = replay_bytes(&bytes).unwrap_err();
        assert!(matches!(err, JournalError::Checksum { .. }));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_magic_is_refused() {
        let err = replay_bytes(b"not a journal").unwrap_err();
        assert!(matches!(err, JournalError::BadMagic));
    }

    #[test]
    fn a_sequence_gap_is_refused_rather_than_papered_over() {
        let path = temp("gap.bin");
        let mut writer = JournalWriter::create(&path).unwrap();
        writer.append(1_800_000_000, 0, b"one").unwrap();
        drop(writer);

        let mut bytes = std::fs::read(&path).unwrap();
        // Forge a second frame whose sequence jumps to 5.
        let mut header = Vec::new();
        header.extend_from_slice(&5u64.to_be_bytes());
        header.extend_from_slice(&1_800_000_001i64.to_be_bytes());
        header.extend_from_slice(&0u32.to_be_bytes());
        header.extend_from_slice(&3u32.to_be_bytes());
        let mut crc = crate::crc::Crc32c::new();
        crc.update(&header);
        crc.update(b"two");
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&crc.finish().to_be_bytes());
        bytes.extend_from_slice(b"two");

        let err = replay_bytes(&bytes).unwrap_err();
        assert!(matches!(
            err,
            JournalError::SequenceGap {
                expected: 2,
                found: 5
            }
        ));
        std::fs::remove_file(&path).ok();
    }
}
