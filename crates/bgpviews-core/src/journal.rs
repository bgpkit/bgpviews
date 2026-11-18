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
//! magic  8   "BGPVJNL1"
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
pub const MAGIC: &[u8; 8] = b"BGPVJNL1";

const HEADER_LEN: usize = 8 + 8 + 8 + 4 + 1 + 4 + 4;

/// One journaled frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournaledFrame {
    /// What this frame is: 0 a complete BMP message, 2 a complete BGP message, 3 a BGP session
    /// event. Section 6 fixes these numbers because a replay has to know what it is reading.
    pub kind: u8,
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
            Self::BadMagic => write!(f, "journal does not start with BGPVJNL1"),
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
    dir: PathBuf,
    index: u64,
    path: PathBuf,
    next_seq: u64,
    written: u64,
    bytes: u64,
    part_started_at: Option<i64>,
    rotate: Rotate,
}

/// When a part is closed and the next one started.
///
/// A journal that never rotates is one file for the life of a session: retention can only
/// expire parts that are closed, so the active one grows without bound and nothing may delete
/// it until the router goes away. Section 6 states both limits; zero means "no limit on this
/// axis", which is what a short-lived test wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rotate {
    /// Bytes after which the current part is closed.
    pub max_bytes: u64,
    /// Seconds a part may stay open.
    pub after_seconds: i64,
}

impl JournalWriter {
    /// Create a new part. The magic is written and flushed immediately, so an
    /// empty part is still identifiable.
    ///
    /// # Errors
    /// Returns [`JournalError::Io`] if the directory or file cannot be created.
    pub fn create(path: &Path) -> Result<Self, JournalError> {
        // The caller names the file: only `create_in` owns the part grammar, so a test that
        // points at a path of its own gets exactly that file.
        let dir = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Self::start(path.to_path_buf(), dir, 0, Rotate::default())
    }

    /// Start part `index` of the journal in `dir`.
    ///
    /// The part is created with `truncate(true)` because a part number is only handed out once
    /// per epoch; a file already at that name is a leftover from a writer that never got to
    /// publish it, and `recovery` is the thing that decides about those, not this constructor.
    ///
    /// # Errors
    /// Returns [`JournalError::Io`] if the directory or the part cannot be created.
    pub fn create_in(dir: &Path, index: u64, rotate: Rotate) -> Result<Self, JournalError> {
        let path = dir.join(format!("journal.{index}.bin"));
        Self::start(path, dir.to_path_buf(), index, rotate)
    }

    /// Open one part at an explicit path, which is what the two constructors share.
    fn start(
        path: PathBuf,
        dir: PathBuf,
        index: u64,
        rotate: Rotate,
    ) -> Result<Self, JournalError> {
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
            .open(&path)
            .map_err(|e| JournalError::Io {
                op: "open part",
                source: e,
            })?;
        let mut writer = Self {
            file: BufWriter::new(file),
            dir,
            index,
            path,
            next_seq: 1,
            written: 0,
            bytes: MAGIC.len() as u64,
            part_started_at: None,
            rotate,
        };
        // Flushed, not just buffered: the part is the record of a session that exists, and a
        // reader that finds it before the first frame must be able to tell what it is.
        writer
            .file
            .write_all(MAGIC)
            .and_then(|()| writer.file.flush())
            .map_err(|e| JournalError::Io {
                op: "write magic",
                source: e,
            })?;
        Ok(writer)
    }

    /// Append one frame to the buffer. Nothing is durable until [`sync`] returns.
    ///
    /// [`sync`]: JournalWriter::sync
    ///
    /// # Errors
    /// Returns [`JournalError::Io`] when the write or the fsync fails.
    pub fn append(
        &mut self,
        seconds: i64,
        micros: u32,
        kind: u8,
        payload: &[u8],
    ) -> Result<u64, JournalError> {
        let len = u32::try_from(payload.len()).map_err(|_| JournalError::Io {
            op: "encode length",
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame too large"),
        })?;
        // Rotate before the frame, never through it: a part boundary is also a batch boundary,
        // so the frame that filled the old part is durable there and the next starts clean.
        if self.should_rotate(u64::from(len) + (HEADER_LEN as u64), seconds)? {
            self.rotate(seconds)?;
        }
        let seq = self.next_seq;

        let mut header = Vec::with_capacity(HEADER_LEN - 8);
        header.extend_from_slice(&seq.to_be_bytes());
        header.extend_from_slice(&seconds.to_be_bytes());
        header.extend_from_slice(&micros.to_be_bytes());
        header.push(kind);
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

        self.next_seq += 1;
        self.written += 1;
        self.bytes += HEADER_LEN as u64 + u64::from(len);
        if self.part_started_at.is_none() {
            self.part_started_at = Some(seconds);
        }
        Ok(seq)
    }

    /// Flush and fsync. One call per batch, not per frame: the design's ingest loop
    /// journals a byte-bounded batch and fsyncs once, and a per-frame fsync turns a
    /// replay of a hundred thousand records into a hundred thousand disc syncs.
    ///
    /// # Errors
    /// Returns [`JournalError::Io`] when the flush or the fsync fails.
    pub fn sync(&mut self) -> Result<(), JournalError> {
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
            })
    }

    /// Whether the frame about to be written belongs in a new part.
    ///
    /// An empty part is never rotated: a part that holds only its magic would be a file that
    /// exists to say nothing, and a limit smaller than one frame would otherwise spin.
    fn should_rotate(&self, frame_bytes: u64, at_seconds: i64) -> Result<bool, JournalError> {
        if self.written == 0 {
            return Ok(false);
        }
        if self.rotate.max_bytes > 0 && self.bytes + frame_bytes > self.rotate.max_bytes {
            return Ok(true);
        }
        match (self.rotate.after_seconds, self.part_started_at) {
            (seconds, Some(started)) if seconds > 0 => Ok(at_seconds - started >= seconds),
            _ => Ok(false),
        }
    }

    /// Close the current part and open the next one, keeping the sequence unbroken.
    fn rotate(&mut self, at_seconds: i64) -> Result<(), JournalError> {
        self.sync()?;
        self.index += 1;
        self.path = self.dir.join(format!("journal.{}.bin", self.index));
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
            .map_err(|e| JournalError::Io {
                op: "open part",
                source: e,
            })?;
        self.file = BufWriter::new(file);
        self.written = 0;
        self.bytes = MAGIC.len() as u64;
        self.part_started_at = Some(at_seconds);
        self.file
            .write_all(MAGIC)
            .and_then(|()| self.file.flush())
            .map_err(|e| JournalError::Io {
                op: "write magic",
                source: e,
            })?;
        self.sync()
    }

    /// Which part this writer is on, counting from zero.
    #[must_use]
    pub fn part_index(&self) -> u64 {
        self.index
    }

    /// Bytes written to this part, including its magic.
    #[must_use]
    pub fn part_bytes(&self) -> u64 {
        self.bytes
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

/// Replay every part of one session's journal, in order, as one frame sequence.
///
/// This is the only check that proves the design's "sequence starts at 1 and has no gaps"
/// across a journal that rotated: a part on its own cannot tell whether it starts where its
/// predecessor ended.
///
/// # Errors
/// As [`replay`], plus [`JournalError::SequenceGap`] when a part does not continue the one
/// before it.
pub fn replay_parts(
    parts: &[PathBuf],
) -> Result<(Vec<JournaledFrame>, JournalSummary), JournalError> {
    let mut frames: Vec<JournaledFrame> = Vec::new();
    let mut torn = false;
    let mut valid = 0usize;
    for part in parts {
        let (found, summary) = replay(part)?;
        for frame in found {
            let expected = frames.last().map_or(1, |previous| previous.seq + 1);
            if frame.seq != expected {
                return Err(JournalError::SequenceGap {
                    expected,
                    found: frame.seq,
                });
            }
            frames.push(frame);
        }
        valid += summary.valid_len;
        torn = summary.torn;
    }
    let count = frames.len() as u64;
    Ok((
        frames,
        JournalSummary {
            valid_len: valid,
            frames: count,
            torn,
        },
    ))
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
        let kind = rest[20];
        let len = u32::from_be_bytes(rest[21..25].try_into().unwrap_or_default()) as usize;
        let checksum = u32::from_be_bytes(rest[25..29].try_into().unwrap_or_default());
        let body_start = offset + (HEADER_LEN - MAGIC.len());
        if rest.len() < (HEADER_LEN - MAGIC.len()) + len {
            torn = true;
            break;
        }
        let payload = &bytes[body_start..body_start + len];

        let mut crc = crate::crc::Crc32c::new();
        // Everything before the checksum, which is the header and the payload.
        crc.update(&rest[0..25]);
        crc.update(payload);
        if crc.finish() != checksum {
            return Err(JournalError::Checksum { offset, seq });
        }
        // A part continues the journal's sequence rather than restarting it, so the first
        // frame may be any number; what must hold inside a part is that each frame follows
        // the one before it. The chain across parts is `replay_parts`' job.
        let expected = frames
            .last()
            .map_or(seq, |frame: &JournaledFrame| frame.seq + 1);
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
            kind,
            payload: payload.to_vec(),
        });
        offset = body_start + len;
        valid_len = offset;
    }

    let count = frames.len() as u64;
    Ok((
        frames,
        JournalSummary {
            valid_len,
            frames: count,
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

    fn parts_in(dir: &Path) -> Vec<PathBuf> {
        let mut found: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let number = name.strip_prefix("journal.")?.strip_suffix(".bin")?;
                Some((number.parse::<u64>().ok()?, entry.path()))
            })
            .collect();
        found.sort();
        found.into_iter().map(|(_, path)| path).collect()
    }

    fn frame_sequence(dir: &Path) -> Vec<u64> {
        let (frames, _summary) = replay_parts(&parts_in(dir)).unwrap();
        frames.iter().map(|frame| frame.seq).collect()
    }

    #[test]
    fn a_part_rotates_at_its_byte_limit_and_the_sequence_stays_whole() {
        let dir =
            std::env::temp_dir().join(format!("bgpviews-rotate-bytes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut writer = JournalWriter::create_in(
            &dir,
            0,
            Rotate {
                max_bytes: 512,
                after_seconds: 0,
            },
        )
        .unwrap();
        for index in 0..20 {
            writer
                .append(1_763_166_900 + index, 0, 0, &[7u8; 64])
                .unwrap();
        }
        writer.sync().unwrap();
        assert!(writer.part_index() >= 1, "the part never rotated");

        let parts = parts_in(&dir);
        assert!(parts.len() >= 2, "{} part(s)", parts.len());
        // Every frame is in exactly one part, in order: rotation is a boundary, not a gap.
        assert_eq!(frame_sequence(&dir), (1..=20).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_part_rotates_when_it_reaches_its_age_limit() {
        let dir = std::env::temp_dir().join(format!("bgpviews-rotate-age-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut writer = JournalWriter::create_in(
            &dir,
            0,
            Rotate {
                max_bytes: 0,
                after_seconds: 60,
            },
        )
        .unwrap();
        let start = 1_763_166_900;
        writer.append(start, 0, 0, b"one").unwrap();
        writer.append(start + 59, 0, 0, b"two").unwrap();
        assert_eq!(writer.part_index(), 0, "59 seconds is inside one part");
        writer.append(start + 60, 0, 0, b"three").unwrap();
        writer.sync().unwrap();
        assert_eq!(writer.part_index(), 1, "a minute closes the part");
        assert_eq!(frame_sequence(&dir), vec![1, 2, 3]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_limits_means_one_part_for_the_whole_session() {
        let dir = std::env::temp_dir().join(format!("bgpviews-rotate-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut writer = JournalWriter::create_in(&dir, 0, Rotate::default()).unwrap();
        for index in 0..8 {
            writer
                .append(1_763_166_900 + index, 0, 0, &[3u8; 128])
                .unwrap();
        }
        writer.sync().unwrap();
        assert_eq!(writer.part_index(), 0);
        assert_eq!(parts_in(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frames_round_trip_with_their_timestamps() {
        let path = temp("round-trip.bin");
        let mut writer = JournalWriter::create(&path).unwrap();
        let first = writer.append(1_800_000_000, 1, 0, b"first").unwrap();
        let second = writer.append(1_800_000_001, 2, 0, b"second").unwrap();
        assert_eq!((first, second), (1, 2));
        writer.sync().unwrap();
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
    fn the_part_names_itself_and_says_what_each_frame_is() {
        let path = temp("layout.bin");
        let mut writer = JournalWriter::create(&path).unwrap();
        writer.append(1_800_000_000, 250_000, 0, b"bmp").unwrap();
        writer.append(1_800_000_001, 0, 2, b"bgp").unwrap();
        writer.sync().unwrap();
        drop(writer);

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], b"BGPVJNL1", "section 6 fixes the magic");
        // Eight bytes of magic, then each frame's fixed part is 29 bytes: sequence, seconds,
        // micros, kind, length, payload, checksum.
        assert_eq!(bytes.len(), 8 + 29 + 3 + 29 + 3);
        assert_eq!(bytes[8 + 20], 0, "the first frame is a BMP message");
        assert_eq!(bytes[8 + 29 + 3 + 20], 2, "the second is a BGP message");

        let (frames, _summary) = replay(&path).unwrap();
        assert_eq!(frames[0].kind, 0);
        assert_eq!(frames[1].kind, 2);
        assert_eq!(frames[1].micros, 0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_part_written_under_the_old_magic_is_refused_by_name() {
        let bytes = b"BGPVPJ01and then some".to_vec();
        assert!(matches!(replay_bytes(&bytes), Err(JournalError::BadMagic)));
    }

    #[test]
    fn a_torn_tail_keeps_the_frames_before_it() {
        let path = temp("torn.bin");
        let mut writer = JournalWriter::create(&path).unwrap();
        writer.append(1_800_000_000, 0, 0, b"kept").unwrap();
        writer.append(1_800_000_001, 0, 0, b"cut-off").unwrap();
        writer.sync().unwrap();
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
        writer.append(1_800_000_000, 0, 0, b"payload").unwrap();
        writer.sync().unwrap();
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
        writer.append(1_800_000_000, 0, 0, b"one").unwrap();
        writer.sync().unwrap();
        drop(writer);

        let mut bytes = std::fs::read(&path).unwrap();
        // Forge a second frame whose sequence jumps to 5.
        let mut header = Vec::new();
        header.extend_from_slice(&5u64.to_be_bytes());
        header.extend_from_slice(&1_800_000_001i64.to_be_bytes());
        header.extend_from_slice(&0u32.to_be_bytes());
        header.push(0); // kind: a BMP message
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
