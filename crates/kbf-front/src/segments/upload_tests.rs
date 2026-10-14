//! The check of one received segment, record by record as ranges arrive: the same
//! built segment whatever the order and cut of the ranges, a refusal of a record whose
//! bytes do not match, and no hashing left for the commit.

use kbf_segments::SegmentWriter;
use tonic::Code;

use super::*;
use crate::MAX_BLOB_BYTES;

/// A segment of `n` blobs of different sizes (the last one `last` bytes) as `Begin`
/// declares it, and its record bytes.
fn segment(n: usize, last: usize) -> (Declared, Vec<u8>) {
    let mut writer = SegmentWriter::new(u64::MAX);
    for i in 0..n {
        let len = if i + 1 == n { last } else { 100 + i * 37 };
        let blob: Vec<u8> = (0..len).map(|b| (b * 7 + i) as u8).collect();
        writer.push(&blob).expect("push");
    }
    let bytes = writer.finish();
    let footer = kbf_segments::Footer::parse(&bytes, bytes.len() as u64).expect("footer");
    let records_len = usize::try_from(footer.records_len()).expect("len");
    let (mut declared, _, _) = declare(
        &[(bytes.len() as u64, &bytes[records_len..])],
        MAX_BLOB_BYTES,
    )
    .expect("declare");
    (declared.remove(0), bytes[..records_len].to_vec())
}

/// Sends `records` in ranges of `cut` bytes in the order `order` names (indices of
/// ranges), as `PutRange` does: write, then run the claimed checks.
fn send(declared: &Declared, records: &[u8], cut: usize, order: &[usize]) -> Receiving {
    let mut received = Receiving::new(declared);
    for &k in order {
        let (start, end) = (k * cut, ((k + 1) * cut).min(records.len()));
        let (_, claimed) = received
            .put(declared, start as u64, &records[start..end])
            .expect("put");
        advance(&claimed, |i, finished| received.next(declared, i, finished));
    }
    received
}

fn in_order(len: usize, cut: usize) -> Vec<usize> {
    (0..len.div_ceil(cut)).collect()
}

/// Catches an incremental check that depends on how the bytes arrive (a hasher fed a
/// later range first, a record spanning ranges hashed short, a CRC of the wrong
/// bytes): ranges of 97, 1000 and 5000 bytes, in order, reversed and interleaved, give
/// the segment built from the whole buffer, with each entry's CRC-32C that of its
/// record, and the records are all checked before any commit.
#[test]
fn any_cut_and_order_builds_the_same_segment() {
    let (declared, records) = segment(30, 3000);
    let mut whole = Receiving::new(&declared);
    let (_, claimed) = whole.put(&declared, 0, &records).expect("put");
    advance(&claimed, |i, f| whole.next(&declared, i, f));
    let want = whole.check(&declared).expect("whole");
    for cut in [97, 1000, 5000] {
        let n = in_order(records.len(), cut);
        let reversed: Vec<usize> = n.iter().rev().copied().collect();
        let evens_then_odds: Vec<usize> = n
            .iter()
            .copied()
            .filter(|k| k % 2 == 0)
            .chain(n.iter().copied().filter(|k| k % 2 == 1))
            .collect();
        for order in [n, reversed, evens_then_odds] {
            let received = send(&declared, &records, cut, &order);
            assert!(!received.checking());
            let built = received.check(&declared).expect("built");
            assert_eq!(built.body, want.body, "cut {cut} order {order:?}");
            assert_eq!(built.entries, want.entries, "cut {cut} order {order:?}");
        }
    }
    for (entry, built) in declared.entries.iter().zip(&want.entries) {
        let range = entry.range();
        let record = &records[range.start as usize..range.end as usize];
        assert_eq!(built.crc32c, kbf_segments::crc32c(record));
    }
}

/// Catches a check that is deferred to the commit (the point of this change): after the
/// last range, every record is already hashed, so `check` hashes nothing and a segment
/// built from records the claims have not run is refused as not checked.
#[test]
fn the_commit_has_nothing_left_to_hash() {
    let (declared, records) = segment(10, 500);
    let mut received = Receiving::new(&declared);
    received.put(&declared, 0, &records).expect("put");
    // Claimed but never advanced, as a commit that raced its ranges would find it.
    assert!(received.checking());
    let raced = received.check(&declared).expect_err("not checked");
    assert_eq!(raced.code(), Code::FailedPrecondition);
    let done = send(&declared, &records, 64, &in_order(records.len(), 64));
    assert!(!done.checking());
    assert!(done.refusal().is_none());
    assert!(done.check(&declared).is_ok());
}

/// Catches a check that stops refusing (the server must still refuse a segment whose
/// bytes do not match their digests), in either cut: a flipped byte in a record that
/// spans ranges and one in a small record are each refused INVALID_ARGUMENT naming the
/// record, at the range that completes it, and `check` refuses too.
#[test]
fn a_corrupt_record_is_refused_when_its_last_byte_arrives() {
    let (declared, records) = segment(20, 3000);
    for victim in [3, 19] {
        let entry = &declared.entries[victim];
        // The entries are in digest order; find the record by its offset.
        let mut bad = records.clone();
        bad[entry.offset as usize + 1] ^= 1;
        for cut in [97, 5000] {
            let received = send(&declared, &bad, cut, &in_order(bad.len(), cut));
            let refused = received.refusal().expect("refused at the range");
            assert_eq!(refused.code(), Code::InvalidArgument, "cut {cut}");
            assert!(
                refused.message().contains(&entry.digest.to_string()),
                "{refused:?}"
            );
            assert_eq!(
                received.check(&declared).expect_err("check").code(),
                Code::InvalidArgument
            );
        }
    }
}

/// Catches a record that is never hashed or hashed twice when ranges are cut inside it
/// (a hasher restarted per range): one record of 10,000 bytes sent in ranges of 1,000
/// is checked, with the CRC-32C of all its bytes.
#[test]
fn a_record_spanning_many_ranges_is_one_hash() {
    let (declared, records) = segment(1, 10_000);
    let received = send(&declared, &records, 1000, &in_order(records.len(), 1000));
    let built = received.check(&declared).expect("built");
    assert_eq!(built.entries[0].crc32c, kbf_segments::crc32c(&records));
}
