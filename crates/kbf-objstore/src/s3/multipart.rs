//! Multipart upload's part sizes (a stand-in: no upload yet).

/// The smallest part S3 takes, except the last: 5 MiB.
pub const MIN_PART_BYTES: u64 = 5 << 20;

/// The largest part S3 takes: 5 GiB, the limit of one PUT.
pub const MAX_PART_BYTES: u64 = 5 << 30;

/// The most parts one object may have.
pub const MAX_PARTS: u64 = 10_000;

/// The part size of any object that fits in [`MAX_PARTS`] of them (156.25 GiB):
/// 16 MiB, what one part holds in memory while it is sent.
pub const PART_BYTES: u64 = 16 << 20;

/// The largest object written with one PUT; a larger one is a multipart upload. One
/// part's worth, so a streamed write of any size holds at most [`PART_BYTES`] (or
/// [`part_size`] of a larger object) of its body at once. Below S3's 5 GiB limit for
/// one PUT.
pub const MULTIPART_THRESHOLD: u64 = PART_BYTES;

const MIB: u64 = 1 << 20;

// One PUT carries at most what S3 takes in one.
const _: () = assert!(MULTIPART_THRESHOLD <= MAX_PART_BYTES);

/// The part size of an object of `len` bytes: [`PART_BYTES`], or for an object too
/// large for [`MAX_PARTS`] parts of that size, the least whole number of MiB that fits
/// it in [`MAX_PARTS`] parts. `None` when even [`MAX_PART_BYTES`] parts do not (an
/// object over 10,000 parts of 5 GiB, about 48.8 TiB).
#[must_use]
pub fn part_size(len: u64) -> Option<u64> {
    // A stand-in until multipart upload lands.
    let _ = (len, MIB);
    Some(PART_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    /// Catches: a part size that breaks S3's rules at the edges: over 10,000 parts (an
    /// object one byte past what 16 MiB parts hold), a part under 5 MiB that is not the
    /// last, a part over 5 GiB, or an object that does fit refused. Every size it gives
    /// is a whole number of MiB, so parts line up with nothing smaller.
    #[test]
    fn part_sizes_keep_s3s_rules_at_the_boundaries() {
        assert_eq!(part_size(1), Some(PART_BYTES));
        assert_eq!(part_size(MULTIPART_THRESHOLD + 1), Some(PART_BYTES));
        let most_at_16_mib = PART_BYTES * MAX_PARTS;
        assert_eq!(part_size(most_at_16_mib), Some(PART_BYTES));
        assert_eq!(part_size(most_at_16_mib + 1), Some(PART_BYTES + MIB));
        // Over 5 GiB, S3's single-PUT limit: still 16 MiB parts.
        assert_eq!(part_size(5 * GIB + 1), Some(PART_BYTES));
        let most = MAX_PART_BYTES * MAX_PARTS;
        assert_eq!(part_size(most), Some(MAX_PART_BYTES));
        assert_eq!(part_size(most + 1), None);
        assert_eq!(part_size(u64::MAX), None);
        for len in [
            MULTIPART_THRESHOLD + 1,
            5 * GIB + 1,
            most_at_16_mib + 1,
            1 << 40,
            most - 1,
            most,
        ] {
            let size = part_size(len).expect("fits");
            assert!(len.div_ceil(size) <= MAX_PARTS, "{len}: too many parts");
            assert!((MIN_PART_BYTES..=MAX_PART_BYTES).contains(&size), "{len}");
            assert_eq!(size % MIB, 0, "{len}");
        }
    }
}
