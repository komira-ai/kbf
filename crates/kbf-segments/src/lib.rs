//! The segment format that packs blobs into stored objects, with integrity checks,
//! and FastCDC content-defined chunking.
//!
//! - [`SegmentWriter`] packs blobs into one segment up to a size limit.
//! - [`SegmentReader`] finds a blob in a whole segment by digest and checks it;
//!   [`Footer`] does the same for a reader that fetches byte ranges.
//! - [`chunks`] and [`Manifest`] split a large blob into content-defined chunks.
//!
//! The layout is documented in [`layout`]. This is a pure crate: no async runtime,
//! network, clock, randomness or hashed collections. The layering test in `kbf-it` and
//! the lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod crc32c;
mod error;
mod fastcdc;
pub mod layout;
mod segment;

use kbf_types::{Digest, DigestFunction};
use sha2::{Digest as _, Sha256};

pub use crate::crc32c::{Crc32c, crc32c};
pub use crate::error::{SegmentError, WriteError};
pub use crate::fastcdc::{
    AVG_CHUNK, CHUNKING_THRESHOLD, Chunks, MAX_CHUNK, MIN_CHUNK, Manifest, chunks,
};
pub use crate::layout::{Footer, IndexEntry};
pub use crate::segment::{MAX_SEGMENT_BYTES, SegmentReader, SegmentWriter};

/// The SHA-256 digest of `bytes`.
#[must_use]
pub fn sha256(bytes: &[u8]) -> Digest {
    Digest::new(
        DigestFunction::Sha256,
        Sha256::digest(bytes).into(),
        bytes.len() as u64,
    )
}

/// The SHA-256 and CRC-32C of one record whose bytes arrive in order, in pieces.
#[derive(Clone, Debug, Default)]
pub struct RecordHasher {
    sha: Sha256,
    crc: Crc32c,
    len: u64,
}

impl RecordHasher {
    /// A hasher of no bytes yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `bytes` after those added before.
    pub fn update(&mut self, bytes: &[u8]) {
        self.sha.update(bytes);
        self.crc.update(bytes);
        self.len += bytes.len() as u64;
    }

    /// The digest and CRC-32C of every byte added: equal to [`sha256`] and [`crc32c`]
    /// of the pieces joined.
    #[must_use]
    pub fn finish(self) -> (Digest, u32) {
        let digest = Digest::new(DigestFunction::Sha256, self.sha.finalize().into(), self.len);
        (digest, self.crc.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a piecewise hasher that differs from the one-shot functions (a length
    /// that counts one piece, a CRC restarted per piece): any split gives `sha256` and
    /// `crc32c` of the whole.
    #[test]
    fn record_hasher_pieces_equal_the_whole() {
        let bytes: Vec<u8> = (0..1000u32).map(|i| (i * 31) as u8).collect();
        for split in [0, 1, 500, 999, 1000] {
            let mut h = RecordHasher::new();
            h.update(&bytes[..split]);
            h.update(&bytes[split..]);
            assert_eq!(
                h.finish(),
                (sha256(&bytes), crc32c(&bytes)),
                "split {split}"
            );
        }
    }
}
