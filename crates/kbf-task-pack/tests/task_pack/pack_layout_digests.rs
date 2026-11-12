//! A pack laid out from its records' digests alone (`PackLayout::plan_digests`), for a writer
//! that has not read its outputs since it hashed them: each CRC-32C is taken from the bytes as
//! they are written, and the pack is byte for byte the one a layout given the CRC-32Cs writes.

use std::collections::BTreeMap;
use std::io;

use kbf_segments::sha256;
use kbf_task_pack::{
    BlobMeta, LayoutError, LayoutWriteError, MAX_RECORD_BYTES, PackLayout, write_pack_with,
};
use kbf_types::{Digest, DigestFunction};

use crate::task_manifest::golden;
use crate::task_pack::golden_blobs;

/// Writes `layout` with each record read from the blob of `blobs` that has its digest; the bytes
/// and the digest `write_to` reported.
fn write(layout: &PackLayout, blobs: &[&[u8]]) -> (Vec<u8>, Digest) {
    let by_digest: BTreeMap<Digest, &[u8]> = blobs.iter().map(|b| (sha256(b), *b)).collect();
    let mut out = Vec::new();
    let digest = layout
        .write_to(&mut out, |d| Ok(by_digest[d]))
        .expect("written");
    (out, digest)
}

/// A blob of `len` bytes that differs with `seed`.
fn blob(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_mul(31).wrapping_add((i % 251) as u8))
        .collect()
}

/// Asserts that the golden manifest and blobs, laid out from their digests and written, are the
/// golden pack's bytes (`write_pack_with`'s), and that sets of generated blobs, an empty one and a
/// repeat among them, written either way are the same bytes with the same object digest.
#[test]
fn a_pack_planned_from_digests_writes_the_bytes_of_one_planned_with_crcs() {
    let json = golden().to_json_with(0).expect("valid");
    let blobs = golden_blobs();
    let mut records: Vec<&[u8]> = vec![&json];
    records.extend(blobs.iter().map(Vec::as_slice));
    let digests: Vec<Digest> = records.iter().map(|r| sha256(r)).collect();
    let layout = PackLayout::plan_digests(&digests).expect("fits");
    let (bytes, digest) = write(&layout, &records);
    let plain: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    assert_eq!(
        bytes,
        write_pack_with(&golden(), &plain, 0).expect("writes")
    );
    assert_eq!(digest, sha256(&bytes));

    let generated = [blob(1, 70_000), blob(2, 3), Vec::new(), blob(3, 65_536)];
    let mut set: Vec<&[u8]> = generated.iter().map(Vec::as_slice).collect();
    set.push(&generated[0]);
    let metas: Vec<BlobMeta> = set.iter().map(|b| BlobMeta::of(b)).collect();
    let digests: Vec<Digest> = set.iter().map(|b| sha256(b)).collect();
    let with_crcs = write(&PackLayout::plan(&metas).expect("fits"), &set);
    let from_digests = write(&PackLayout::plan_digests(&digests).expect("fits"), &set);
    assert_eq!(from_digests, with_crcs);
}

/// Asserts that a layout from digests keeps the caps of one with CRC-32Cs: a record one byte
/// past the record cap and a pack of no record are refused the same way.
#[test]
fn a_pack_planned_from_digests_keeps_the_caps() {
    let over = Digest::new(DigestFunction::Sha256, [1; 32], MAX_RECORD_BYTES + 1);
    assert_eq!(
        PackLayout::plan_digests(&[over]),
        Err(LayoutError::RecordTooLarge {
            size: MAX_RECORD_BYTES + 1
        })
    );
    assert_eq!(
        PackLayout::plan_digests(&[sha256(b"")]),
        Err(LayoutError::NoRecords)
    );
}

/// Asserts that with no CRC-32C to check, a source without its record's bytes (one byte short,
/// or one byte other) is still refused as changed, by the record's digest, and nothing past it
/// is written.
#[test]
fn a_changed_source_is_refused_when_planned_from_digests() {
    let (a, b) = (blob(4, 70_000), blob(5, 50));
    let layout = PackLayout::plan_digests(&[sha256(&a), sha256(&b)]).expect("fits");
    let short = a[..a.len() - 1].to_vec();
    let mut other = a.clone();
    other[40_000] ^= 1;
    for (what, source) in [("short", short), ("other bytes", other)] {
        let mut out = Vec::new();
        let mut opened = Vec::new();
        let err = layout
            .write_to(&mut out, |d| {
                opened.push(*d);
                Ok(io::Cursor::new(if *d == sha256(&a) {
                    source.clone()
                } else {
                    b.clone()
                }))
            })
            .expect_err(what);
        assert!(
            matches!(err, LayoutWriteError::Changed { digest } if digest == sha256(&a)),
            "{what}: {err:?}"
        );
        assert_eq!(opened, [sha256(&a)], "{what}");
    }
}
