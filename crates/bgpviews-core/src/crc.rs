//! CRC-32C (Castagnoli), used to frame every line of the coverage log.
//!
//! Implemented here rather than pulled from a crate because the coverage log is
//! the one file a third party may have to read without our code, and the fewer
//! moving parts behind its checksum the better. Polynomial 0x1EDC6F41, reflected
//! form 0x82F63B78, which is what `CRC-32C` names everywhere else.

const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let poly: u32 = 0x82F6_3B78;
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ poly
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Streaming CRC-32C state.
#[derive(Debug, Clone, Copy)]
pub struct Crc32c(u32);

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32c {
    /// A fresh state, before any byte is fed.
    #[must_use]
    pub const fn new() -> Self {
        Self(0xFFFF_FFFF)
    }

    /// Feed bytes into the checksum.
    pub fn update(&mut self, bytes: &[u8]) {
        let mut crc = self.0;
        for byte in bytes {
            let index = ((crc ^ u32::from(*byte)) & 0xFF) as usize;
            crc = TABLE[index] ^ (crc >> 8);
        }
        self.0 = crc;
    }

    /// The finished checksum.
    #[must_use]
    pub const fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

/// The CRC-32C of a single buffer.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut state = Crc32c::new();
    state.update(bytes);
    state.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The check value every CRC-32C implementation is expected to produce, so an
    /// independent implementer can verify this one without reading it.
    #[test]
    fn known_vector() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn streaming_matches_one_shot() {
        let data = b"updates.20261114.1200.gz";
        let mut state = Crc32c::new();
        for chunk in data.chunks(3) {
            state.update(chunk);
        }
        assert_eq!(state.finish(), crc32c(data));
    }
}
