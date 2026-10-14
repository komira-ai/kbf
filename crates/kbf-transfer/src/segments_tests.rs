//! The upload plan, the range cutting and the read coalescing, without a server.

use kbf_segments::layout::footer_len;
use kbf_segments::{Footer, IndexEntry};
use kbf_types::{Digest as KbfDigest, DigestFunction};

use super::*;

/// A spooled blob of `size` bytes at `offset` whose hash starts with `n`.
fn spooled(n: u8, size: u64, offset: u64) -> Spooled {
    let mut hash = [0u8; 32];
    hash[0] = n;
    hash[31] = n.wrapping_mul(7);
    Spooled {
        digest: KbfDigest::new(DigestFunction::Sha256, hash, size),
        offset,
        crc32c: u32::from(n),
    }
}

/// `n` blobs of `size` bytes, back to back in the spool.
fn back_to_back(n: u8, size: u64) -> Vec<Spooled> {
    (0..n)
        .map(|i| spooled(i, size, u64::from(i) * size))
        .collect()
}

/// Catches an upload cut at every segment (mutant: one upload per segment) and a
/// segment over its limit: blobs past one segment's limit fill several segments of
/// one upload, each within the limit.
#[test]
fn segments_fill_to_their_limit_within_one_upload() {
    let limit = 1000 + footer_len(4);
    let uploads = plan(&back_to_back(10, 250), limit, u64::MAX, usize::MAX);
    assert_eq!(uploads.len(), 1);
    let sizes: Vec<usize> = uploads[0].iter().map(|s| s.records.len()).collect();
    assert_eq!(sizes, [4, 4, 2]);
    assert!(uploads[0].iter().all(|s| s.length() <= limit));
}

/// Catches an upload over the server's byte cap or record cap (it would be refused):
/// a new upload starts before either is passed.
#[test]
fn uploads_stay_under_both_caps() {
    let by_bytes = plan(
        &back_to_back(10, 100),
        u64::MAX,
        300 + footer_len(3),
        usize::MAX,
    );
    let counts: Vec<usize> = by_bytes.iter().map(|u| u[0].records.len()).collect();
    assert_eq!(counts, [3, 3, 3, 1]);
    let by_records = plan(&back_to_back(10, 100), u64::MAX, u64::MAX, 4);
    let counts: Vec<usize> = by_records.iter().map(|u| u[0].records.len()).collect();
    assert_eq!(counts, [4, 4, 2]);
}

/// Catches a blob too large for a segment dropped, or packed with others: it is a
/// segment of one record, and the segment after it starts fresh.
#[test]
fn a_blob_over_the_segment_limit_is_a_segment_of_its_own() {
    let records = [
        spooled(1, 10, 0),
        spooled(2, 5000, 10),
        spooled(3, 10, 5010),
    ];
    let uploads = plan(&records, 1000, u64::MAX, usize::MAX);
    let sizes: Vec<usize> = uploads[0].iter().map(|s| s.records.len()).collect();
    assert_eq!(sizes, [1, 1, 1]);
    assert_eq!(uploads[0][1].length(), 5000 + footer_len(1));
}

/// Catches a footer the server would refuse (unsorted, wrong offsets or length) and a
/// range read from the wrong place in the spool: the footer parses for the segment's
/// length and names each blob at its segment offset; ranges cover the record area in
/// pieces that map to the spool, adjacent pieces joined.
#[test]
fn footers_parse_and_ranges_map_to_the_spool() {
    // Spool order 3, 1, 2: the footer is sorted by digest, offsets follow the spool.
    let records = [spooled(3, 100, 0), spooled(1, 50, 500), spooled(2, 70, 550)];
    let segment = &plan(&records, u64::MAX, u64::MAX, usize::MAX)[0][0];
    let footer = Footer::parse(&segment.footer(), segment.length()).expect("parse");
    let offsets: Vec<(u8, u64)> = footer
        .entries()
        .iter()
        .map(|e: &IndexEntry| (e.digest.hash[0], e.offset))
        .collect();
    assert_eq!(offsets, [(1, 100), (2, 150), (3, 0)]);
    assert_eq!(segment.ranges(120), [(0, 120), (120, 220)]);
    // 0..100 is spool 0..100; 100..120 is the start of blob 1 at spool 500.
    assert_eq!(segment.pieces(0, 120), [(0, 100), (500, 20)]);
    // Blobs 1 and 2 are adjacent in the spool: one piece.
    assert_eq!(segment.pieces(120, 220), [(520, 100)]);
}

/// Catches fills still one call per blob, or a run that passes the read limit or
/// reads across a wide gap: close blobs of one object share a run, a far one or one
/// of another object starts another, and a run never grows past the limit.
#[test]
fn coalescing_joins_close_blobs_of_one_object() {
    let object = |seq| ObjectRef { epoch: 1, seq };
    let d = |n: u8, size: i64| Digest {
        hash: format!("{n:02x}"),
        size_bytes: size,
    };
    let located = vec![
        (object(1), 0, d(1, 100)),
        (object(1), 150, d(2, 100)),
        (object(1), 10_000, d(3, 100)),
        (object(2), 0, d(4, 100)),
        (object(1), 250, d(5, 1000)),
    ];
    let runs = coalesce(located, 1000, 1200);
    let shape: Vec<(u64, u64, u64, usize)> = runs
        .iter()
        .map(|r| (r.object.seq, r.start, r.end, r.blobs.len()))
        .collect();
    assert_eq!(
        shape,
        [
            (1, 0, 250, 2),
            (1, 250, 1250, 1),
            (1, 10_000, 10_100, 1),
            (2, 0, 100, 1)
        ]
    );
}
