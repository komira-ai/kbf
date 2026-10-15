//! Hashing the records of a block or an unverified segment against the digests the
//! index lists them under (`docs/design/write-path.md`, section 6.7).

use kbf_segments::IndexEntry;
use kbf_types::Digest;

/// A record that does not hash to the digest it is listed under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Mismatch {
    /// The digest the record was listed under.
    pub claimed: Digest,
    /// The digest of the bytes found (of no bytes, if the record lies past `records`).
    pub actual: Digest,
}

/// What hashing a block's records found.
#[derive(Debug, Default)]
pub(super) struct Verified {
    /// Every record that hashed to its digest, with its CRC-32C and offset, in the
    /// order of the `blobs` hashed.
    pub good: Vec<IndexEntry>,
    /// Every record that did not.
    pub bad: Vec<Mismatch>,
}

/// Hashes (SHA-256 and CRC-32C) each of `blobs`, digests with their offsets in
/// `records`, and sorts them into [`Verified::good`] and [`Verified::bad`]. A record
/// not wholly inside `records` is bad. CPU-bound: run it on the hashing pool.
pub(super) fn verify_records(records: &[u8], blobs: &[(Digest, u64)]) -> Verified {
    let mut out = Verified::default();
    for (digest, offset) in blobs {
        let span = usize::try_from(*offset)
            .ok()
            .zip(usize::try_from(digest.size_bytes).ok())
            .and_then(|(from, len)| Some(from..from.checked_add(len)?));
        let record = span.and_then(|span| records.get(span)).unwrap_or_default();
        let actual = kbf_segments::sha256(record);
        if actual == *digest {
            out.good.push(IndexEntry {
                digest: *digest,
                offset: *offset,
                crc32c: kbf_segments::crc32c(record),
            });
        } else {
            out.bad.push(Mismatch {
                claimed: *digest,
                actual,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(bytes: &[u8], at: u64) -> (Digest, u64) {
        (kbf_segments::sha256(bytes), at)
    }

    /// Catches a check that stops at the first bad record (the good ones after it
    /// lost), one that passes a record whose bytes changed, one that panics or passes
    /// a record running past the block, and a CRC taken of other bytes than the
    /// record's.
    #[test]
    fn records_are_sorted_into_good_and_bad_by_their_own_bytes() {
        let records = b"firstSECONDthird".to_vec();
        let list = [blob(b"first", 0), blob(b"seconX", 5), blob(b"third", 11)];
        let found = verify_records(&records, &list);
        let good: Vec<_> = found.good.iter().map(|e| (e.digest, e.offset)).collect();
        assert_eq!(good, [list[0], list[2]]);
        assert_eq!(found.good[1].crc32c, kbf_segments::crc32c(b"third"));
        assert_eq!(found.bad.len(), 1);
        assert_eq!(found.bad[0].claimed, list[1].0);
        assert_eq!(found.bad[0].actual, kbf_segments::sha256(b"SECOND"));
        let past = verify_records(&records, &[blob(b"third!", 11), blob(b"x", u64::MAX)]);
        assert!(past.good.is_empty());
        assert_eq!(past.bad.len(), 2);
        assert_eq!(past.bad[1].actual, kbf_segments::sha256(b""));
    }
}
