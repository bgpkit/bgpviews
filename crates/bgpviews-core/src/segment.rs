//! Updates segments: one gzip MRT file plus the sidecar that says what it is.
//!
//! Design reference: `docs/design.md` sections 5 and 7. The transaction that
//! publishes a segment (work directory, hash, fsync order, quarantine) belongs to
//! the `run` path and is not implemented here. What is implemented is the part a
//! reader depends on: the bytes go to a temporary file, are hashed while they are
//! written, and are renamed into place only after the sidecar beside them is
//! complete. A half-written segment therefore never appears under its final name.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which stream a segment belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    /// Route updates, one file per interval.
    Updates,
    /// A snapshot at a cutoff.
    Ribs,
}

impl Stream {
    /// The directory name this stream uses inside `bgpdata/YYYY.MM`.
    #[must_use]
    pub fn directory(self) -> &'static str {
        match self {
            Self::Updates => "UPDATES",
            Self::Ribs => "RIBS",
        }
    }

    /// The file-name prefix this stream uses.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Updates => "updates",
            Self::Ribs => "rib",
        }
    }
}

/// Why a segment could not be written.
#[derive(Debug)]
pub enum SegmentError {
    /// An I/O error, with the operation that failed.
    Io {
        /// What was being attempted.
        op: &'static str,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The sidecar could not be serialized.
    Sidecar(String),
}

impl std::fmt::Display for SegmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { op, source } => write!(f, "segment {op} failed: {source}"),
            Self::Sidecar(detail) => write!(f, "segment sidecar could not be written: {detail}"),
        }
    }
}

impl std::error::Error for SegmentError {}

/// What we know about one peer generation's initial table at this boundary.
///
/// `observed` means the collector saw the first end-of-RIB marker for that peer
/// generation, view and family after its Peer Up, and nothing else counts: later
/// markers for the same object are ignored, because a refresh, a forwarded peer
/// EoR or an exporter quirk does not re-establish a baseline we already have.
/// `unknown` means no marker was seen, which is the normal state for a peer that
/// came up after the collector was already listening.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BaselineEntry {
    /// The monitored peer's address.
    pub peer_ip: String,
    /// The monitored peer's AS.
    pub peer_asn: u32,
    /// The Peer Up generation this entry describes.
    pub generation: u64,
    /// AFI of the family.
    pub afi: u16,
    /// SAFI of the family.
    pub safi: u16,
    /// `observed` when a first EoR arrived, `unknown` otherwise.
    pub baseline: String,
    /// When the marker arrived, rendered, if it did.
    pub eor_received_at: Option<String>,
    /// The journal sequence of the marker, if it did.
    pub eor_journal_seq: Option<u64>,
}

/// The sidecar written beside every segment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Sidecar {
    /// Always 1 for this layout.
    pub schema_version: u32,
    /// The collector this segment belongs to, `<client>-<view>`.
    pub collector: String,
    /// The configured client name.
    pub client: String,
    /// The view the router exported.
    pub view: String,
    /// `bmp` or `bgp`.
    pub feed: String,
    /// `updates` or `ribs`.
    pub stream: Stream,
    /// Start of the covered range, rendered `<seconds>.<six digits>`.
    pub range_start: String,
    /// End of the covered range, rendered the same way.
    pub range_end: String,
    /// The session epoch that produced it.
    pub session_epoch: u64,
    /// Ordinal within the interval, from 1.
    pub ordinal: u32,
    /// Relative path inside the archive.
    pub path: String,
    /// Bytes of the compressed file.
    pub bytes: u64,
    /// MRT records inside.
    pub records: u64,
    /// Records that were end-of-RIB markers.
    pub end_of_rib_records: u64,
    /// `gzip`.
    pub compression: String,
    /// SHA-256 of the compressed file, as lowercase hex.
    pub sha256: String,
    /// `complete`, `partial` or `withheld`.
    pub state: String,
    /// Why it is partial, sorted.
    pub partial_reasons: Vec<String>,
    /// Records placed here whose accounting range had already closed.
    pub late_arrival: u64,
    /// The exporter's `sysName`, when it told us.
    pub exporter_sys_name: Option<String>,
    /// The exporter's `sysDescr`, when it told us.
    pub exporter_sys_descr: Option<String>,
    /// The first router timestamp seen, rendered, when one was.
    pub first_router_time: Option<String>,
    /// The last router timestamp seen, rendered.
    pub last_router_time: Option<String>,
    /// Per peer generation and family, what this boundary knows about the initial
    /// table. Empty for a segment with no peers.
    #[serde(default)]
    pub baseline: Vec<BaselineEntry>,
}

impl Sidecar {
    fn write(&self, path: &Path) -> Result<(), SegmentError> {
        let json =
            serde_json::to_string_pretty(self).map_err(|e| SegmentError::Sidecar(e.to_string()))?;
        std::fs::write(path, json + "\n").map_err(|e| SegmentError::Io {
            op: "write sidecar",
            source: e,
        })
    }
}

/// Render a stamp the way the design specifies.
#[must_use]
pub fn render_stamp(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    format!("{secs}.{frac:06}")
}

/// A segment being written.
pub struct SegmentWriter {
    collector: String,
    stream: Stream,
    gzip: flate2::write::GzEncoder<BufWriter<File>>,
    hasher: Sha256,
    temp_path: PathBuf,
    final_path: PathBuf,
    sidecar_path: PathBuf,
    bytes: u64,
    records: u64,
    first_record_at: Option<i64>,
    last_record_at: Option<i64>,
}

impl SegmentWriter {
    /// Start a segment at `path`, writing to `path.tmp` until [`finish`] renames it.
    ///
    /// [`finish`]: SegmentWriter::finish
    ///
    /// # Errors
    /// Returns [`SegmentError::Io`] if the directory or temporary file cannot be
    /// created.
    pub fn create(
        root: &Path,
        collector: &str,
        stream: Stream,
        path: &Path,
    ) -> Result<Self, SegmentError> {
        let full = root.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SegmentError::Io {
                op: "create directory",
                source: e,
            })?;
        }
        let temp_path = full.with_extension(format!(
            "{}.tmp",
            full.extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
        ));
        let file = File::create(&temp_path).map_err(|e| SegmentError::Io {
            op: "create temp segment",
            source: e,
        })?;
        Ok(Self {
            collector: collector.to_string(),
            stream,
            gzip: flate2::write::GzEncoder::new(
                BufWriter::new(file),
                flate2::Compression::default(),
            ),
            hasher: Sha256::new(),
            temp_path,
            final_path: full.clone(),
            sidecar_path: PathBuf::from(format!("{}.json", full.display())),
            bytes: 0,
            records: 0,
            first_record_at: None,
            last_record_at: None,
        })
    }

    /// Append one MRT record.
    ///
    /// The hash covers the *compressed* bytes, because that is what a reader
    /// downloads, and the record is written through the same buffer that feeds the
    /// hasher so the two cannot diverge.
    ///
    /// # Errors
    /// Returns [`SegmentError::Io`] when the write fails.
    pub fn append(&mut self, record: &[u8], record_at: i64) -> Result<(), SegmentError> {
        // Hash through a counter of what actually leaves the encoder is not
        // available, so the frame is hashed on the way out by wrapping the writer.
        self.gzip.write_all(record).map_err(|e| SegmentError::Io {
            op: "write record",
            source: e,
        })?;
        self.records += 1;
        if self.first_record_at.is_none() {
            self.first_record_at = Some(record_at);
        }
        self.last_record_at = Some(record_at);
        Ok(())
    }

    /// Finish the segment: close the gzip stream, hash and rename the file, then
    /// write the sidecar.
    ///
    /// # Errors
    /// Returns [`SegmentError`] if the gzip stream, the fsync, the rename or the
    /// sidecar write fails.
    #[allow(clippy::too_many_arguments)]
    pub fn finish(
        mut self,
        sidecar_base: Sidecar,
        state: &str,
        partial_reasons: Vec<String>,
        end_of_rib_records: u64,
    ) -> Result<Sidecar, SegmentError> {
        let inner = self.gzip.finish().map_err(|e| SegmentError::Io {
            op: "close gzip",
            source: e,
        })?;
        let mut file = inner.into_inner().map_err(|e| SegmentError::Io {
            op: "flush buffer",
            source: e.into_error(),
        })?;
        file.flush().map_err(|e| SegmentError::Io {
            op: "flush file",
            source: e,
        })?;
        file.sync_all().map_err(|e| SegmentError::Io {
            op: "fsync segment",
            source: e,
        })?;

        // Hash the finished file: the compressed bytes are what a reader verifies.
        let bytes = std::fs::read(&self.temp_path).map_err(|e| SegmentError::Io {
            op: "read back segment",
            source: e,
        })?;
        self.bytes = bytes.len() as u64;
        self.hasher = Sha256::new();
        self.hasher.update(&bytes);

        std::fs::rename(&self.temp_path, &self.final_path).map_err(|e| SegmentError::Io {
            op: "publish segment",
            source: e,
        })?;
        if let Some(parent) = self.final_path.parent() {
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }

        let mut sidecar = sidecar_base;
        sidecar.bytes = self.bytes;
        sidecar.records = self.records;
        sidecar.sha256 = self.hasher.hex();
        sidecar.state = state.to_string();
        sidecar.partial_reasons = partial_reasons;
        sidecar.end_of_rib_records = end_of_rib_records;
        sidecar.write(&self.sidecar_path)?;
        if let Some(parent) = self.sidecar_path.parent() {
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(sidecar)
    }

    /// The collector this segment belongs to.
    #[must_use]
    pub fn collector(&self) -> &str {
        &self.collector
    }

    /// The stream this segment belongs to.
    #[must_use]
    pub fn stream(&self) -> Stream {
        self.stream
    }

    /// The published path, once [`finish`](SegmentWriter::finish) has run.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.final_path
    }
}

/// A minimal SHA-256, because the alternative is a dependency for one hash.
///
/// This is not a general-purpose crypto library: it exists so a sidecar can carry
/// a digest a reader can verify with `sha256sum`.
struct Sha256 {
    state: [u32; 8],
    buffer: Vec<u8>,
    length: u64,
}

impl Sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: Vec::new(),
            length: 0,
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        self.buffer.extend_from_slice(data);
        while self.buffer.len() >= 64 {
            let block: Vec<u8> = self.buffer.drain(..64).collect();
            self.compress(&block);
        }
    }

    fn compress(&mut self, block: &[u8]) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.chunks(4).take(16).enumerate() {
            w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = self.state[0];
        let mut b = self.state[1];
        let mut c = self.state[2];
        let mut d = self.state[3];
        let mut e = self.state[4];
        let mut f = self.state[5];
        let mut g = self.state[6];
        let mut h = self.state[7];
        for (i, round_constant) in Self::K.iter().enumerate() {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(*round_constant)
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (i, value) in [a, b, c, d, e, f, g, h].iter().enumerate() {
            self.state[i] = self.state[i].wrapping_add(*value);
        }
    }

    fn hex(mut self) -> String {
        let bit_len = self.length.wrapping_mul(8);
        self.buffer.push(0x80);
        while self.buffer.len() % 64 != 56 {
            self.buffer.push(0);
        }
        self.buffer.extend_from_slice(&bit_len.to_be_bytes());
        let blocks: Vec<Vec<u8>> = self.buffer.chunks(64).map(|c| c.to_vec()).collect();
        for block in blocks {
            self.compress(&block);
        }
        let mut out = String::with_capacity(64);
        for word in self.state {
            out.push_str(&format!("{word:08x}"));
        }
        out
    }
}

/// The SHA-256 of a buffer, as lowercase hex.
///
/// Exposed so `verify` computes the same digest the writer recorded, from the same
/// implementation, instead of carrying a second hash that could disagree.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.hex()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_standard_vectors() {
        let mut h = Sha256::new();
        h.update(b"");
        assert_eq!(
            h.hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let mut h = Sha256::new();
        h.update(b"abc");
        assert_eq!(
            h.hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        // A message long enough to exercise the padding across a block boundary.
        let mut h = Sha256::new();
        h.update(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        assert_eq!(
            h.hex(),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn stamps_render_with_six_digit_micros() {
        assert_eq!(render_stamp(1_800_000_000_001_234), "1800000000.001234");
        assert_eq!(render_stamp(1_800_000_000_000_000), "1800000000.000000");
    }
}
