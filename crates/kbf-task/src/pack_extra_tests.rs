use kbf_segments::{SegmentReader, sha256};
use kbf_task_pack::{MAX_PACK_RECORDS, MAX_RECORD_BYTES};

use super::*;

/// Catches a pack 1.. holding a manifest, a skipped or repeated blob, or an empty record: the
/// records are exactly the distinct non-empty blobs, the first record at offset 0 is the first
/// blob, and the pack reads back by digest.
#[test]
fn an_extra_pack_holds_each_distinct_blob_once_and_no_manifest() {
    let pack = build_extra(&[b"one", b"", b"two", b"one"]).expect("pack");
    let reader = SegmentReader::open(&pack).expect("segment");
    assert_eq!(reader.footer().entries().len(), 2);
    assert_eq!(reader.get(&sha256(b"one")).expect("get"), Some(&b"one"[..]));
    assert_eq!(reader.get(&sha256(b"two")).expect("get"), Some(&b"two"[..]));
    assert_eq!(&pack[..3], b"one");
}

/// Catches a pack with nothing in it being refused: it is a valid segment of no record.
#[test]
fn an_extra_pack_of_no_blob_is_empty() {
    let pack = build_extra(&[]).expect("pack");
    let reader = SegmentReader::open(&pack).expect("segment");
    assert!(reader.footer().entries().is_empty());
}

/// Catches a record over the record cap being written to a pack (a daemon never reads it from
/// one), and the cap being off by one: exactly the cap is accepted.
#[test]
fn an_extra_pack_refuses_a_record_over_the_cap() {
    let at = vec![1u8; usize::try_from(MAX_RECORD_BYTES).expect("fits")];
    build_extra(&[&at]).expect("exactly the cap");
    let over = vec![1u8; at.len() + 1];
    let err = build_extra(&[&over]).unwrap_err();
    assert_eq!(err.detail, OUTPUT_TOO_LARGE);
    assert!(err.to_string().contains("record may hold"), "{err}");
}

/// Catches a pack with more records than ingest reads being written: 16,384 records are
/// accepted, 16,385 are refused as too large.
#[test]
fn an_extra_pack_refuses_more_records_than_the_cap() {
    let blobs: Vec<Vec<u8>> = (0..=MAX_PACK_RECORDS as u32)
        .map(|n| n.to_be_bytes().to_vec())
        .collect();
    let views: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    build_extra(&views[..MAX_PACK_RECORDS]).expect("exactly the cap");
    let err = build_extra(&views).unwrap_err();
    assert_eq!(err.detail, OUTPUT_TOO_LARGE);
    assert!(err.to_string().contains("16385 records"), "{err}");
}

/// Catches records passing the pack's bytes being accepted: the writer's refusal is a size cap.
#[test]
fn an_extra_pack_refuses_records_past_the_pack_bytes() {
    let each = vec![2u8; usize::try_from(MAX_RECORD_BYTES).expect("fits")];
    let mut second = each.clone();
    second[0] = 3;
    let err = build_extra(&[&each, &second, &[4u8; 3]]).unwrap_err();
    assert_eq!(err.detail, OUTPUT_TOO_LARGE);
}

/// Catches a plan refusal losing its detail: a size cap is `output-too-large` and a wrong size
/// is `invalid-manifest`.
#[test]
fn a_plan_refusal_keeps_its_detail() {
    let too_large = PackError::from(PlanError::TooLarge("x".to_owned()));
    assert_eq!(too_large.detail, OUTPUT_TOO_LARGE);
    let mismatch = PackError::from(PlanError::SizeMismatch {
        given: 1,
        digest: 2,
    });
    assert_eq!(mismatch.detail, INVALID_MANIFEST);
}
