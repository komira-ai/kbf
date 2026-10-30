//! The shape of a task pack, reading its manifest and writing one: the golden pack under
//! `tests/fixtures/task_pack`, footers made to fail one rule each, and packs built to fail
//! reading.

use kbf_segments::{
    Footer, IndexEntry, SegmentError, SegmentReader, SegmentWriter, WriteError, layout, sha256,
};
use kbf_task_pack::{
    MAX_MANIFEST_BYTES, MAX_PACK_BYTES, MAX_PACK_RECORDS, MAX_RECORD_BYTES, ManifestError,
    ShapeError, TaskManifest, TaskPackError, WritePackError, check_records, check_shape,
    read_manifest, read_manifest_with, write_pack, write_pack_with,
};
use kbf_types::{Digest, DigestFunction};

use crate::task_manifest::{fixture, golden, golden_pack};

// The blobs of the golden pack, in the order they were pushed after the manifest.
const A_O: &[u8] = b"object code of a\n";
const TOOL: &[u8] = b"an executable output of the action\n";
const STDOUT: &[u8] = b"compiling a.o\n";
const STDERR: &[u8] = b"warning: unused variable\n";
const X: &[u8] = b"x inside the directory\n";

/// The `Tree` of `out/d` as a REAPI proto: a root directory with one file node, `x`.
fn tree() -> Vec<u8> {
    let x = sha256(X).hash_hex();
    [&b"\nK\nI\n\x01x\x12D\n@"[..], x.as_bytes(), b"\x10\x17"].concat()
}

/// The blobs of the golden pack in the order they were written after the manifest: the offsets
/// in the fixture follow it.
fn golden_blobs() -> Vec<Vec<u8>> {
    vec![
        A_O.to_vec(),
        TOOL.to_vec(),
        STDOUT.to_vec(),
        STDERR.to_vec(),
        tree(),
        X.to_vec(),
    ]
}

/// A footer that indexes the records `(size, offset)`, for an object whose records end at
/// `records_len`. Digests are made up (the first bytes count up), so the footer can describe
/// records far bigger than any test could hold.
fn footer(records: &[(u64, u64)], records_len: u64) -> Footer {
    let mut entries: Vec<IndexEntry> = records
        .iter()
        .enumerate()
        .map(|(i, &(size, offset))| {
            let mut hash = [0u8; 32];
            hash[..4].copy_from_slice(&u32::try_from(i + 1).expect("small").to_be_bytes());
            IndexEntry {
                digest: Digest::new(DigestFunction::Sha256, hash, size),
                offset,
                crc32c: 0,
            }
        })
        .collect();
    entries.sort_by_key(|e| e.digest);
    let bytes = layout::encode_footer(records_len, &entries);
    let total = records_len + bytes.len() as u64;
    Footer::parse(&bytes, total).expect("a footer that parses")
}

fn shape(records: &[(u64, u64)], records_len: u64) -> Result<u64, ShapeError> {
    let f = footer(records, records_len);
    check_shape(&f, records_len + layout::footer_len(records.len() as u64))
        .map(|e| e.digest.size_bytes)
}

/// Asserts that the golden pack is a task pack whose footer passes the shape check, whose
/// record at offset 0 is the manifest (the bytes of the manifest fixture, the value the
/// fixture parses to), whose records are the blobs and trees the manifest names plus the
/// blob the tree names, and that the empty output file has no record.
#[test]
fn the_golden_pack_parses_and_its_manifest_is_at_offset_zero() {
    let pack = golden_pack();
    let manifest_json = fixture("task_pack_v1.manifest.json");
    let reader = SegmentReader::open(&pack).expect("a segment");
    let entry = check_shape(reader.footer(), pack.len() as u64).expect("a task pack");
    assert_eq!(entry.offset, 0);
    assert_eq!(entry.digest, sha256(&manifest_json));
    assert_eq!(reader.footer().entries().len(), 7);
    assert!(
        reader
            .footer()
            .entries()
            .iter()
            .all(|e| e.digest.size_bytes > 0)
    );

    let manifest = read_manifest(&pack).expect("the manifest");
    assert_eq!(manifest, golden());
    for named in manifest.output_digests() {
        assert!(
            reader.footer().find(&named).is_some(),
            "{named} has a record"
        );
    }
    let tree = tree();
    assert!(reader.footer().find(&sha256(&tree)).is_some());
    assert!(
        reader.footer().find(&sha256(X)).is_some(),
        "a file the tree names"
    );
}

/// Asserts that writing the golden manifest and blobs gives the golden pack's bytes, so the
/// writer and the fixture cannot drift, and that empty blobs and repeated blobs change nothing.
#[test]
fn writing_the_golden_manifest_gives_the_golden_pack() {
    let blobs = golden_blobs();
    let plain: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    assert_eq!(
        write_pack(&golden(), &plain).expect("writes"),
        golden_pack()
    );

    let mut noisy = vec![&b""[..]];
    noisy.extend(&plain);
    noisy.push(A_O);
    noisy.push(b"");
    assert_eq!(
        write_pack(&golden(), &noisy).expect("writes"),
        golden_pack()
    );
}

/// Asserts that a pack written for a manifest the reader would refuse is not written.
#[test]
fn a_pack_for_an_invalid_manifest_is_not_written() {
    let mut manifest = golden();
    manifest.lease.clear();
    assert!(matches!(
        write_pack(&manifest, &[]),
        Err(WritePackError::Manifest(ManifestError::Lease(_)))
    ));
}

/// Asserts that a blob over 64 MiB is refused when writing (an output over a cap is a task
/// failure, never a truncation), and that blobs which together pass 128 MiB are refused as
/// a segment that is full.
#[test]
fn a_blob_over_a_record_or_blobs_over_a_pack_are_not_written() {
    let record = usize::try_from(MAX_RECORD_BYTES).expect("fits");
    let too_big = vec![0u8; record + 1];
    assert!(matches!(
        write_pack(&golden(), &[&too_big]),
        Err(WritePackError::RecordTooLarge { size }) if size == MAX_RECORD_BYTES + 1
    ));
    let first = vec![0u8; record];
    let second = vec![0u8; record - 1];
    assert!(matches!(
        write_pack(&golden(), &[&first, &second]),
        Err(WritePackError::Segment(WriteError::Full { .. }))
    ));
}

/// Asserts that a record of size 0 is refused, at offset 0 and elsewhere.
#[test]
fn a_zero_size_record_is_refused() {
    assert_eq!(
        shape(&[(0, 0), (5, 0)], 5),
        Err(ShapeError::ZeroSizeRecord { offset: 0 })
    );
    assert_eq!(
        shape(&[(5, 0), (0, 5)], 5),
        Err(ShapeError::ZeroSizeRecord { offset: 5 })
    );
}

/// Asserts that two zero-size records at one offset are refused for being zero-size, not as a
/// gap or an overlap: one defect, one reason.
#[test]
fn two_zero_size_records_at_one_offset_are_refused() {
    assert_eq!(
        shape(&[(0, 0), (0, 0), (5, 0)], 5),
        Err(ShapeError::ZeroSizeRecord { offset: 0 })
    );
}

/// Asserts that records must be back to back from offset 0 to the footer: a gap, an overlap, a
/// first record that does not start at 0 and a last that ends short of the footer are refused.
#[test]
fn a_gap_an_overlap_or_a_misplaced_end_is_refused() {
    let not_contiguous = |expected, found| Err(ShapeError::NotContiguous { expected, found });
    assert_eq!(shape(&[(5, 0), (5, 6)], 11), not_contiguous(5, 6));
    assert_eq!(shape(&[(5, 0), (5, 4)], 9), not_contiguous(5, 4));
    assert_eq!(shape(&[(5, 1)], 6), not_contiguous(0, 1));
    assert_eq!(shape(&[(5, 0)], 9), not_contiguous(5, 9));
    assert_eq!(shape(&[(5, 0), (4, 5)], 9), Ok(5));
}

/// Asserts that the manifest is the record at offset 0 whatever the order of the footer's index
/// (which is by hash): the size returned is that of the record that starts at 0.
#[test]
fn the_manifest_is_the_record_at_offset_zero_not_the_first_in_the_index() {
    // The made-up hashes ascend with the position in the slice, so offset 0 is last here.
    assert_eq!(shape(&[(7, 9), (9, 0)], 16), Ok(9));
}

/// Asserts the record cap: 64 MiB is a record, one byte more is refused.
#[test]
fn a_record_over_64_mib_is_refused() {
    let at = MAX_RECORD_BYTES;
    assert_eq!(shape(&[(10, 0), (at, 10)], 10 + at), Ok(10));
    assert_eq!(
        shape(&[(10, 0), (at + 1, 10)], 11 + at),
        Err(ShapeError::RecordTooLarge {
            offset: 10,
            size: at + 1
        })
    );
}

/// Asserts the manifest cap: a record of 4 MiB at offset 0 is a manifest, one byte more is not.
#[test]
fn a_manifest_record_over_four_mebibytes_is_refused() {
    let at = MAX_MANIFEST_BYTES;
    assert_eq!(shape(&[(at, 0)], at), Ok(at));
    assert_eq!(
        shape(&[(at + 1, 0)], at + 1),
        Err(ShapeError::ManifestTooLarge { size: at + 1 })
    );
}

/// Asserts the count cap: 16384 records are a pack, 16385 are not, and no record is no manifest.
#[test]
fn the_record_count_is_one_to_16384() {
    let many = |n: usize| -> Vec<(u64, u64)> { (0..n as u64).map(|i| (1, i)).collect() };
    assert_eq!(
        shape(&many(MAX_PACK_RECORDS), MAX_PACK_RECORDS as u64),
        Ok(1)
    );
    assert_eq!(
        shape(&many(MAX_PACK_RECORDS + 1), MAX_PACK_RECORDS as u64 + 1),
        Err(ShapeError::TooManyRecords {
            count: MAX_PACK_RECORDS + 1
        })
    );
    assert_eq!(shape(&[], 0), Err(ShapeError::NoRecords));
}

/// Asserts the object cap: 128 MiB is a pack, one byte more is refused by length alone.
#[test]
fn an_object_over_128_mib_is_refused() {
    let f = footer(&[(5, 0)], 5);
    assert!(check_shape(&f, MAX_PACK_BYTES).is_ok());
    assert_eq!(
        check_shape(&f, MAX_PACK_BYTES + 1),
        Err(ShapeError::TooLarge {
            len: MAX_PACK_BYTES + 1
        })
    );
}

/// Asserts that an object that is not a segment is a segment error, and that damage to the
/// manifest record is found: the pack is read through the footer's CRC and digest.
#[test]
fn a_damaged_pack_is_refused_as_a_segment() {
    assert!(matches!(
        read_manifest(b"not a segment at all, but long enough for a trailer......"),
        Err(TaskPackError::Segment(SegmentError::BadMagic))
    ));
    let mut pack = golden_pack();
    pack[0] ^= 1;
    assert!(matches!(
        read_manifest(&pack),
        Err(TaskPackError::Segment(SegmentError::RecordCrc { .. }))
    ));
}

/// Asserts that a segment that is not a task pack is refused for the shape: an empty record
/// first, so offset 0 has no manifest, and an empty record after one.
#[test]
fn a_segment_with_a_zero_size_record_is_not_a_task_pack() {
    let manifest = golden().to_json().expect("json");
    let mut first_empty = SegmentWriter::new(MAX_PACK_BYTES);
    first_empty.push(b"").expect("pushed");
    first_empty.push(&manifest).expect("pushed");
    assert!(matches!(
        read_manifest(&first_empty.finish()),
        Err(TaskPackError::Shape(ShapeError::ZeroSizeRecord {
            offset: 0
        }))
    ));
    let mut later_empty = SegmentWriter::new(MAX_PACK_BYTES);
    later_empty.push(&manifest).expect("pushed");
    later_empty.push(b"").expect("pushed");
    assert!(matches!(
        read_manifest(&later_empty.finish()),
        Err(TaskPackError::Shape(ShapeError::ZeroSizeRecord { .. }))
    ));
}

/// Asserts that a record at offset 0 that is not a manifest is refused as one: the first record
/// pushed is what the pack's manifest is, so a blob first is a pack without a manifest.
#[test]
fn a_pack_whose_first_record_is_not_a_manifest_is_refused() {
    let manifest = golden().to_json().expect("json");
    let mut writer = SegmentWriter::new(MAX_PACK_BYTES);
    writer.push(b"object code, not a manifest").expect("pushed");
    writer.push(&manifest).expect("pushed");
    assert!(matches!(
        read_manifest(&writer.finish()),
        Err(TaskPackError::Manifest(ManifestError::Json(_)))
    ));
}

/// Asserts that a manifest the reader refuses is refused through the pack too: the pack of a
/// manifest with a `..` path, written by hand, around the writer's own check.
#[test]
fn a_pack_with_an_invalid_manifest_is_refused() {
    let bad = golden().to_json().expect("json");
    let bad = String::from_utf8(bad)
        .expect("utf8")
        .replace("out/a.o", "../a.o");
    let mut writer = SegmentWriter::new(MAX_PACK_BYTES);
    writer.push(bad.as_bytes()).expect("pushed");
    writer.push(A_O).expect("pushed");
    assert!(matches!(
        read_manifest(&writer.finish()),
        Err(TaskPackError::Manifest(ManifestError::Path { .. }))
    ));
}

/// Asserts that the manifest value read from a pack made by the writer is the one written.
#[test]
fn a_written_pack_reads_back_to_its_manifest() {
    let manifest: TaskManifest = golden();
    let pack = write_pack(&manifest, &[A_O]).expect("writes");
    assert_eq!(read_manifest(&pack).expect("reads"), manifest);
}

/// Asserts that a pack that holds no manifest (packs 1.. of a run) may start with a record of any
/// size up to the record cap: `check_records` accepts a first record over the manifest cap and
/// returns it, where `check_shape` refuses it as a manifest.
#[test]
fn a_first_record_over_the_manifest_cap_is_a_record_but_not_a_manifest() {
    let big = MAX_MANIFEST_BYTES + 1;
    let f = footer(&[(big, 0), (7, big)], big + 7);
    let len = big + 7 + layout::footer_len(2);

    let first = check_records(&f, len).expect("records");
    assert_eq!((first.offset, first.digest.size_bytes), (0, big));
    assert_eq!(
        check_shape(&f, len),
        Err(ShapeError::ManifestTooLarge { size: big })
    );
}

/// Asserts that `check_records` keeps every other rule of the shape: no record, too many, a zero
/// size record, a record over the cap, a gap, records that stop short of the footer, an object
/// over the cap. A relaxed manifest bound must not relax these.
#[test]
fn check_records_keeps_the_layout_rules() {
    let records = |recs: &[(u64, u64)], end: u64| {
        let f = footer(recs, end);
        check_records(&f, end + layout::footer_len(recs.len() as u64)).map(|e| e.offset)
    };
    assert_eq!(records(&[], 0), Err(ShapeError::NoRecords));
    assert_eq!(
        records(&[(0, 0), (5, 0)], 5),
        Err(ShapeError::ZeroSizeRecord { offset: 0 })
    );
    assert_eq!(
        records(&[(MAX_RECORD_BYTES + 1, 0)], MAX_RECORD_BYTES + 1),
        Err(ShapeError::RecordTooLarge {
            offset: 0,
            size: MAX_RECORD_BYTES + 1
        })
    );
    assert_eq!(
        records(&[(5, 0), (5, 6)], 11),
        Err(ShapeError::NotContiguous {
            expected: 5,
            found: 6
        })
    );
    assert_eq!(
        records(&[(5, 0)], 6),
        Err(ShapeError::NotContiguous {
            expected: 5,
            found: 6
        })
    );
    let many: Vec<(u64, u64)> = (0..=MAX_PACK_RECORDS as u64).map(|i| (1, i)).collect();
    assert_eq!(
        records(&many, MAX_PACK_RECORDS as u64 + 1),
        Err(ShapeError::TooManyRecords {
            count: MAX_PACK_RECORDS + 1
        })
    );
    let f = footer(&[(5, 0)], 5);
    assert_eq!(
        check_records(&f, MAX_PACK_BYTES + 1),
        Err(ShapeError::TooLarge {
            len: MAX_PACK_BYTES + 1
        })
    );
}

/// Asserts that a pack whose manifest names two packs is written and read only by the `_with`
/// forms: `write_pack` and `read_manifest` refuse it (`MultiPack`), so a single-pack consumer
/// cannot take pack 0 for the whole run.
#[test]
fn a_multi_pack_manifest_is_written_and_read_only_by_the_with_forms() {
    let manifest = TaskManifest {
        packs: 2,
        ..golden()
    };
    assert!(matches!(
        write_pack(&manifest, &[]),
        Err(WritePackError::Manifest(ManifestError::MultiPack(2)))
    ));
    let pack = write_pack_with(&manifest, &[], 0).expect("written");
    assert!(matches!(
        read_manifest(&pack),
        Err(TaskPackError::Manifest(ManifestError::MultiPack(2)))
    ));
    assert_eq!(read_manifest_with(&pack, 0).expect("read"), manifest);
}
