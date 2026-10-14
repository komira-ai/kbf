//! CRC-32C (Castagnoli), the per-record and footer checksum of a segment.
//!
//! A table-driven software implementation: the result must not depend on which CPU
//! computes it, and a scrub is bounded by object store reads, not by this loop.

/// The reflected Castagnoli polynomial.
const POLY: u32 = 0x82f6_3b78;

const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        #[allow(clippy::cast_possible_truncation)] // i < 256
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// The CRC-32C of `bytes`.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = Crc32c::new();
    crc.update(bytes);
    crc.finish()
}

/// A CRC-32C over bytes that arrive in pieces: equal to [`crc32c`] of the pieces
/// joined.
#[derive(Clone, Copy, Debug)]
pub struct Crc32c(u32);

impl Crc32c {
    /// The CRC of no bytes yet.
    #[must_use]
    pub const fn new() -> Self {
        Self(!0)
    }

    /// Adds `bytes` after those added before.
    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            let low = self.0.to_le_bytes()[0];
            self.0 = TABLE[usize::from(low ^ b)] ^ (self.0 >> 8);
        }
    }

    /// The CRC-32C of every byte added.
    #[must_use]
    pub const fn finish(self) -> u32 {
        !self.0
    }
}

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::crc32c;

    /// Catches: a wrong polynomial (plain CRC-32 gives 0xcbf43926 for "123456789"), a
    /// missing initial or final inversion, or a non-reflected table. The vectors are the
    /// standard check value and the RFC 3720 (iSCSI) test patterns.
    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(&[0u8; 32]), 0x8a91_36aa);
        assert_eq!(crc32c(&[0xffu8; 32]), 0x62a8_ab43);
        let ascending: Vec<u8> = (0u8..32).collect();
        assert_eq!(crc32c(&ascending), 0x46dd_794e);
    }

    /// Catches an incremental CRC that restarts, or inverts, at each piece: any split of
    /// the bytes gives the one-shot value.
    #[test]
    fn pieces_give_the_whole_crc() {
        let bytes: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        for split in [0, 1, 7, 500, 999, 1000] {
            let mut crc = super::Crc32c::default();
            crc.update(&bytes[..split]);
            crc.update(&bytes[split..]);
            assert_eq!(crc.finish(), crc32c(&bytes), "split at {split}");
        }
    }
}
