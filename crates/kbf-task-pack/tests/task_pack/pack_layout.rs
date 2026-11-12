//! Laying out a pack from digests and writing it from sources: the bytes equal those of the
//! in-memory writers (`write_pack_with` for pack 0, `SegmentWriter` for the packs without a
//! manifest), the caps refuse at their bounds, and every way a source or the sink fails stops the
//! write with its own error.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use kbf_segments::{SegmentWriter, layout::footer_len, sha256};
use kbf_task_pack::{
    BlobMeta, LayoutError, LayoutWriteError, MAX_PACK_BYTES, MAX_PACK_RECORDS, MAX_RECORD_BYTES,
    PackLayout, TaskManifest, read_manifest, read_manifest_with, write_pack_with,
};
use kbf_types::{Digest, DigestFunction};

use crate::task_manifest::{LARGE_MAX, golden, golden_pack, parse_v2};
use crate::task_pack::golden_blobs;

/// The metas of `blobs`, in order.
fn metas(blobs: &[&[u8]]) -> Vec<BlobMeta> {
    blobs.iter().map(|b| BlobMeta::of(b)).collect()
}

/// Writes `layout` with each record read from the blob of `blobs` that has its digest, and
/// returns the bytes and the digest `write_to` reported.
fn write(layout: &PackLayout, blobs: &[&[u8]]) -> (Vec<u8>, Digest) {
    let by_digest: BTreeMap<Digest, &[u8]> = blobs.iter().map(|b| (sha256(b), *b)).collect();
    let mut out = Vec::new();
    let digest = layout
        .write_to(&mut out, |d| Ok(by_digest[d]))
        .expect("written");
    (out, digest)
}

/// Lays out and writes pack 0 for `manifest` and `blobs`, as a streaming writer does: the
/// manifest record first, then the blobs.
fn streamed_pack_0(manifest: &TaskManifest, blobs: &[&[u8]], large_max: u64) -> Vec<u8> {
    let json = manifest.to_json_with(large_max).expect("a valid manifest");
    let mut records: Vec<&[u8]> = vec![&json];
    records.extend(blobs);
    let layout = PackLayout::plan(&metas(&records)).expect("fits");
    let (bytes, digest) = write(&layout, &records);
    assert_eq!(digest, sha256(&bytes), "the digest of the whole object");
    bytes
}

/// A blob of `len` bytes that differs with `seed`.
fn blob(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6_364_136_223_846_793_005) | 1;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 56) as u8
        })
        .collect()
}

/// A made-up meta of `size` bytes, distinct for each `n`, for a layout that is never written.
fn fake(n: u32, size: u64) -> BlobMeta {
    let mut hash = [0u8; 32];
    hash[..4].copy_from_slice(&n.to_be_bytes());
    BlobMeta {
        digest: Digest::new(DigestFunction::Sha256, hash, size),
        crc32c: n,
    }
}

/// Asserts that the golden manifest and blobs, laid out and streamed, are the golden pack and the
/// bytes `write_pack_with` writes, and that empty and repeated blobs (a repeat of the manifest's
/// own bytes too) change nothing, as they change nothing for `write_pack_with`.
#[test]
fn the_golden_pack_streamed_is_the_golden_pack() {
    let blobs = golden_blobs();
    let plain: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    let streamed = streamed_pack_0(&golden(), &plain, 0);
    assert_eq!(streamed, golden_pack());
    assert_eq!(
        streamed,
        write_pack_with(&golden(), &plain, 0).expect("writes")
    );

    let json = golden().to_json_with(0).expect("valid");
    let mut noisy: Vec<&[u8]> = vec![b""];
    noisy.extend(&plain);
    noisy.extend([&b""[..], plain[0], plain[3], &json]);
    assert_eq!(streamed_pack_0(&golden(), &noisy, 0), golden_pack());
    assert_eq!(
        write_pack_with(&golden(), &noisy, 0).expect("writes"),
        golden_pack()
    );
    assert_eq!(read_manifest(&streamed).expect("reads"), golden());
}

/// Asserts that a version 2 manifest (several packs, large objects, written under a large cap)
/// streams to the bytes `write_pack_with` writes under the same cap, and reads back.
#[test]
fn a_v2_manifest_streamed_equals_write_pack_with() {
    let manifest = parse_v2("valid_v2/packs_and_large.json").expect("parses");
    let blobs = [blob(1, 300), blob(2, 70_000)];
    let views: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    let streamed = streamed_pack_0(&manifest, &views, LARGE_MAX);
    assert_eq!(
        streamed,
        write_pack_with(&manifest, &views, LARGE_MAX).expect("writes")
    );
    assert_eq!(
        read_manifest_with(&streamed, LARGE_MAX).expect("reads"),
        manifest
    );
}

/// Asserts, over generated sets of blobs (sizes from 0 to past three copy buffers, repeats,
/// empties, first hash bytes spread over the fan-out), that the layout written is byte for byte
/// `write_pack_with`'s pack 0 and the `SegmentWriter` segment of a pack with no manifest, in
/// every order the set is given in.
#[test]
fn generated_blob_sets_stream_to_the_in_memory_writers_bytes() {
    let sizes = [0usize, 1, 2, 31, 4096, 65_535, 65_536, 65_537, 200_003];
    for seed in 0..12u64 {
        let count = 1 + (seed as usize * 7) % 40;
        let mut blobs: Vec<Vec<u8>> = (0..count as u64)
            .map(|i| {
                blob(
                    seed * 1000 + i,
                    sizes[((seed + i) % sizes.len() as u64) as usize],
                )
            })
            .collect();
        // A repeat, so the dedup applies, and the order the in-memory writers see matters.
        blobs.push(blobs[count / 2].clone());
        for reversed in [false, true] {
            let mut views: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
            if reversed {
                views.reverse();
            }

            let expected_0 = write_pack_with(&golden(), &views, 0).expect("writes");
            assert_eq!(
                streamed_pack_0(&golden(), &views, 0),
                expected_0,
                "pack 0, seed {seed}, reversed {reversed}"
            );

            let mut segment = SegmentWriter::new(MAX_PACK_BYTES);
            for b in views.iter().filter(|b| !b.is_empty()) {
                segment.push(b).expect("fits");
            }
            let expected = segment.finish();
            let Ok(layout) = PackLayout::plan(&metas(&views)) else {
                // Only a set of empty blobs has no layout, and its segment holds no record.
                assert_eq!(expected.len() as u64, footer_len(0), "seed {seed}");
                continue;
            };
            let (bytes, digest) = write(&layout, &views);
            assert_eq!(
                bytes, expected,
                "pack 1.., seed {seed}, reversed {reversed}"
            );
            assert_eq!(digest, sha256(&expected), "seed {seed}");
        }
    }
}

/// Asserts that `open` is called once per record, in offset order, and never for an empty blob
/// or a repeat: each source is read once.
#[test]
fn each_record_is_opened_once_in_offset_order() {
    let (a, b, c) = (blob(1, 10), blob(2, 100_000), blob(3, 5));
    let views: Vec<&[u8]> = vec![&b, b"", &a, &b, &c, &a];
    let layout = PackLayout::plan(&metas(&views)).expect("fits");
    let mut opened = Vec::new();
    let mut out = Vec::new();
    layout
        .write_to(&mut out, |d| {
            opened.push(*d);
            Ok([&a, &b, &c]
                .into_iter()
                .find(|x| sha256(x) == *d)
                .expect("a blob of the set")
                .as_slice())
        })
        .expect("written");
    assert_eq!(opened, vec![sha256(&b), sha256(&a), sha256(&c)]);
}

/// Asserts the record cap: a blob of exactly `MAX_RECORD_BYTES` is laid out, one byte more is
/// refused with its size, even after a valid blob; a blob of size 0 is skipped, whatever its
/// position.
#[test]
fn the_record_cap_refuses_one_byte_past_it() {
    assert!(PackLayout::plan(&[fake(1, MAX_RECORD_BYTES)]).is_ok());
    assert_eq!(
        PackLayout::plan(&[fake(1, 10), fake(2, 0), fake(3, MAX_RECORD_BYTES + 1)]),
        Err(LayoutError::RecordTooLarge {
            size: MAX_RECORD_BYTES + 1
        })
    );
}

/// Asserts the record count cap: `MAX_PACK_RECORDS` distinct blobs are laid out, one more is
/// refused with the count, and a repeat does not count.
#[test]
fn the_record_count_cap_refuses_one_record_past_it() {
    let mut blobs: Vec<BlobMeta> = (0..MAX_PACK_RECORDS as u32).map(|n| fake(n, 1)).collect();
    blobs.push(fake(0, 1));
    assert!(PackLayout::plan(&blobs).is_ok());
    blobs.push(fake(MAX_PACK_RECORDS as u32, 1));
    assert_eq!(
        PackLayout::plan(&blobs),
        Err(LayoutError::TooManyRecords {
            count: MAX_PACK_RECORDS + 1
        })
    );
}

/// Asserts the pack cap, footer counted: records that with their footer make exactly
/// `MAX_PACK_BYTES` are laid out, one byte more is refused with the length.
#[test]
fn the_pack_cap_counts_the_footer_and_refuses_one_byte_past_it() {
    let rest = MAX_PACK_BYTES - MAX_RECORD_BYTES - footer_len(2);
    assert!(PackLayout::plan(&[fake(1, MAX_RECORD_BYTES), fake(2, rest)]).is_ok());
    assert_eq!(
        PackLayout::plan(&[fake(1, MAX_RECORD_BYTES), fake(2, rest + 1)]),
        Err(LayoutError::TooLarge {
            len: MAX_PACK_BYTES + 1
        })
    );
}

/// Asserts that no blob, or only empty ones, is no pack: a pack with no record has no manifest
/// and readers refuse it.
#[test]
fn no_record_is_refused() {
    assert_eq!(PackLayout::plan(&[]), Err(LayoutError::NoRecords));
    assert_eq!(
        PackLayout::plan(&[fake(1, 0), fake(2, 0)]),
        Err(LayoutError::NoRecords)
    );
}

/// The layout of `[a, b]` for two generated blobs, and the blobs.
fn two() -> (PackLayout, Vec<u8>, Vec<u8>) {
    let (a, b) = (blob(10, 70_000), blob(11, 50));
    let layout = PackLayout::plan(&metas(&[&a, &b])).expect("fits");
    (layout, a, b)
}

/// Writes `layout` with `a`'s source replaced by `source_a`, and `b` read from `b`.
fn write_with<R: Read>(
    layout: &PackLayout,
    a: &[u8],
    b: &[u8],
    mut source_a: impl FnMut() -> io::Result<R>,
) -> Result<Vec<u8>, LayoutWriteError> {
    let mut out = Vec::new();
    layout.write_to(&mut out, |d| {
        if *d == sha256(a) {
            source_a().map(|r| Box::new(r) as Box<dyn Read>)
        } else {
            Ok(Box::new(io::Cursor::new(b.to_vec())) as Box<dyn Read>)
        }
    })?;
    Ok(out)
}

/// Asserts that a source shorter than its record, one of the right length with other bytes, a
/// meta whose CRC-32C is not the bytes', and a meta whose CRC-32C is the bytes' but whose digest
/// is not are each refused as changed, naming the record.
#[test]
fn a_source_without_its_records_bytes_is_refused_as_changed() {
    let (layout, a, b) = two();
    let short = a[..a.len() - 1].to_vec();
    let mut other = a.clone();
    other[40_000] ^= 1;
    for (what, source) in [("short", short), ("other bytes", other)] {
        let err =
            write_with(&layout, &a, &b, || Ok(io::Cursor::new(source.clone()))).expect_err(what);
        assert!(
            matches!(err, LayoutWriteError::Changed { digest } if digest == sha256(&a)),
            "{what}: {err:?}"
        );
    }

    let wrong_crc = BlobMeta {
        crc32c: BlobMeta::of(&b).crc32c ^ 1,
        ..BlobMeta::of(&b)
    };
    let layout = PackLayout::plan(&[BlobMeta::of(&a), wrong_crc]).expect("fits");
    let err = write_with(&layout, &a, &b, || Ok(io::Cursor::new(a.clone()))).expect_err("crc");
    assert!(matches!(err, LayoutWriteError::Changed { digest } if digest == sha256(&b)));

    // A meta whose CRC-32C and size are the source's but whose digest is another blob's.
    let mut other = a.clone();
    other[0] ^= 1;
    let forged = BlobMeta {
        digest: sha256(&other),
        ..BlobMeta::of(&a)
    };
    let layout = PackLayout::plan(&[forged]).expect("fits");
    let mut out = Vec::new();
    let err = layout
        .write_to(&mut out, |_| Ok(a.as_slice()))
        .expect_err("digest");
    assert!(matches!(err, LayoutWriteError::Changed { digest } if digest == sha256(&other)));
}

/// Asserts that a source longer than its record gives exactly the record's bytes: what is read is
/// the record's size, and the pack is the one the blobs alone give.
#[test]
fn bytes_past_a_records_size_are_not_read() {
    let (layout, a, b) = two();
    let longer = [a.as_slice(), b"appended after hashing"].concat();
    let out = write_with(&layout, &a, &b, || Ok(io::Cursor::new(longer.clone()))).expect("writes");
    assert_eq!(out, write(&layout, &[&a, &b]).0);
}

/// A reader that answers `Interrupted` before every read it passes on.
struct Interrupting<R> {
    inner: R,
    interrupt: bool,
}

impl<R: Read> Read for Interrupting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.interrupt = !self.interrupt;
        if self.interrupt {
            return Err(io::ErrorKind::Interrupted.into());
        }
        self.inner.read(buf)
    }
}

/// Asserts that a source interrupted before every read is read again, and the pack is the same.
#[test]
fn an_interrupted_read_is_read_again() {
    let (layout, a, b) = two();
    let out = write_with(&layout, &a, &b, || {
        Ok(Interrupting {
            inner: io::Cursor::new(a.clone()),
            interrupt: false,
        })
    })
    .expect("writes");
    assert_eq!(out, write(&layout, &[&a, &b]).0);
}

/// A reader that gives `good` bytes, then fails.
struct FailsAfter {
    good: usize,
}

impl Read for FailsAfter {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.good == 0 {
            return Err(io::Error::other("the disk went away"));
        }
        let n = buf.len().min(self.good);
        buf[..n].fill(7);
        self.good -= n;
        Ok(n)
    }
}

/// Asserts that a source that cannot be opened, and one that fails part way, stop the write with
/// the record's digest and the source's own error.
#[test]
fn a_source_that_fails_to_open_or_read_names_the_record() {
    let (layout, a, b) = two();
    let err = write_with(&layout, &a, &b, || {
        Err::<io::Empty, _>(io::ErrorKind::NotFound.into())
    })
    .expect_err("open");
    assert!(
        matches!(&err, LayoutWriteError::Source { digest, source }
            if *digest == sha256(&a) && source.kind() == io::ErrorKind::NotFound),
        "{err:?}"
    );

    let err = write_with(&layout, &a, &b, || Ok(FailsAfter { good: 65_536 })).expect_err("read");
    assert!(
        matches!(&err, LayoutWriteError::Source { digest, source }
            if *digest == sha256(&a) && source.to_string() == "the disk went away"),
        "{err:?}"
    );
}

/// A sink that refuses its `fail_at`-th write (counting from 1) and takes every other.
struct RefusesOne {
    fail_at: usize,
    writes: usize,
    bytes: Vec<u8>,
}

impl Write for RefusesOne {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        if self.writes == self.fail_at {
            return Err(io::Error::other("no room"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Asserts that a sink that refuses one write stops the write there, with the sink's error:
/// refused inside the first record, the second record is never opened; refused at the footer,
/// after both records went out, the error is still returned. The sink takes every write after
/// the refused one, so a write that went on past the error would succeed and fail the test.
#[test]
fn a_sink_that_refuses_a_write_stops_it() {
    let (layout, a, b) = two();
    let run = |fail_at: usize| {
        let mut sink = RefusesOne {
            fail_at,
            writes: 0,
            bytes: Vec::new(),
        };
        let mut opened = Vec::new();
        let result = layout.write_to(&mut sink, |d| {
            opened.push(*d);
            Ok(if *d == sha256(&a) {
                a.as_slice()
            } else {
                b.as_slice()
            })
        });
        (result, opened, sink.writes)
    };
    let (whole, opened, writes) = run(0);
    whole.expect("a sink that refuses nothing takes the pack");
    assert_eq!(opened, vec![sha256(&a), sha256(&b)]);

    let (in_record, opened, _) = run(1);
    let err = in_record.expect_err("refused in the first record");
    assert!(
        matches!(&err, LayoutWriteError::Sink(e) if e.to_string() == "no room"),
        "{err:?}"
    );
    assert_eq!(opened, vec![sha256(&a)], "the write stopped in record a");

    // The footer is the last write of a whole pack.
    let (at_footer, opened, _) = run(writes);
    let err = at_footer.expect_err("refused at the footer");
    assert!(
        matches!(&err, LayoutWriteError::Sink(e) if e.to_string() == "no room"),
        "{err:?}"
    );
    assert_eq!(opened, vec![sha256(&a), sha256(&b)]);
}

/// Asserts the text of every refusal, which names the cap, the size or the record.
#[test]
fn every_refusal_says_what_and_which() {
    let d = sha256(b"x");
    let cases = [
        (
            LayoutError::NoRecords.to_string(),
            "the pack would hold no record".to_owned(),
        ),
        (
            LayoutError::RecordTooLarge { size: 67_108_865 }.to_string(),
            "a blob of 67108865 bytes is more than the 67108864 a record may hold".to_owned(),
        ),
        (
            LayoutError::TooManyRecords { count: 16_385 }.to_string(),
            "the pack would hold 16385 records, more than 16384".to_owned(),
        ),
        (
            LayoutError::TooLarge { len: 134_217_729 }.to_string(),
            "the pack would have 134217729 bytes, more than 134217728".to_owned(),
        ),
        (
            LayoutWriteError::Source {
                digest: d,
                source: io::Error::other("gone"),
            }
            .to_string(),
            format!("reading the record {d}: gone"),
        ),
        (
            LayoutWriteError::Changed { digest: d }.to_string(),
            format!("the source of the record {d} no longer has its bytes"),
        ),
        (
            LayoutWriteError::Sink(io::Error::other("full")).to_string(),
            "writing the pack: full".to_owned(),
        ),
    ];
    for (got, want) in cases {
        assert_eq!(got, want);
    }
}
