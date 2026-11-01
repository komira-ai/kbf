//! Digests: the index's [`BlobDigest`] and the segment format's [`Digest`] are the same
//! pair (SHA-256 and length); these convert between them and check bytes against one.

use kbf_pack_index::BlobDigest;
use kbf_segments::sha256;
use kbf_types::Digest;

/// The digest of `bytes`: their SHA-256 and their length.
#[must_use]
pub fn blob_digest(bytes: &[u8]) -> BlobDigest {
    from_segment(&sha256(bytes))
}

/// The index's form of a segment digest.
pub(crate) fn from_segment(digest: &Digest) -> BlobDigest {
    BlobDigest {
        hash: digest.hash,
        size: digest.size_bytes,
    }
}

/// Whether `bytes` are the blob `digest` names.
pub(crate) fn verifies(digest: &BlobDigest, bytes: &[u8]) -> bool {
    blob_digest(bytes) == *digest
}
