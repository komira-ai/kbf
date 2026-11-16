//! The one-pass task object: a stored ZIP64 archive. What the writer writes, what a pure-Rust ZIP
//! reader makes of it, and what the reader here accepts and refuses before it trusts the index.

use std::io::{self, Cursor, Read, Write};

use kbf_segments::sha256;
use kbf_task_pack::one_pass::{
    INDEX_NAME, MANIFEST_NAME, ObjectError, ObjectWriter, TAIL_LEN, Tail, Trailer, Written,
    looks_like,
};

/// A blob of `len` bytes that differs with `seed`.
fn blob(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// Writes `blobs` (each streamed in pieces of `piece` bytes when `piece` is not 0, else given
/// whole), then the manifest `manifest` and the index `files`.
fn write_with(blobs: &[&[u8]], manifest: &[u8], files: &[u8], piece: usize) -> (Vec<u8>, Written) {
    let mut writer = ObjectWriter::new(Vec::new());
    for bytes in blobs {
        let digest = sha256(bytes);
        if piece == 0 {
            writer.blob(&digest, bytes).expect("written");
        } else {
            let mut at = writer.begin().expect("begun");
            for part in bytes.chunks(piece) {
                writer.chunk(&mut at, part).expect("chunk");
            }
            writer.end(at, &digest).expect("ended");
        }
    }
    writer.finish(manifest, files).expect("finished")
}

fn write(blobs: &[&[u8]], manifest: &[u8], piece: usize) -> (Vec<u8>, Written) {
    write_with(blobs, manifest, b"{\"v\":1}", piece)
}

/// The reader's two steps over `object`.
fn read(object: &[u8]) -> Result<Tail, ObjectError> {
    let len = object.len() as u64;
    let trailer = Trailer::parse(&object[object.len().saturating_sub(TAIL_LEN)..], len)?;
    let tail = &object[object.len() - usize::try_from(trailer.tail_len()).expect("fits")..];
    Tail::parse(&trailer, tail)
}

#[test]
fn objects_round_trip_each_blob_at_its_offset() {
    let blobs = [blob(1, 10), blob(2, 3000), blob(3, 1)];
    let refs: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    let (object, written) = write(&refs, b"{\"manifest\":1}", 0);
    let tail = read(&object).expect("read");
    assert_eq!(tail.manifest, b"{\"manifest\":1}");
    assert_eq!(tail.files, b"{\"v\":1}");
    assert_eq!(tail.entries.len(), 3);
    assert_eq!((written.blobs, written.dead_bytes), (3, 0));
    for bytes in &blobs {
        let entry = tail
            .entries
            .iter()
            .find(|e| e.digest == sha256(bytes))
            .expect("entry");
        let range = entry.range();
        assert_eq!(&object[range.start as usize..range.end as usize], bytes);
        assert!(entry.verify(bytes));
        assert!(!entry.verify(&bytes[1..]));
    }
    assert!(tail.entries.windows(2).all(|p| p[0].digest < p[1].digest));
    assert_eq!(written.object, sha256(&object));
    assert!(looks_like(&object[object.len() - TAIL_LEN..]));
    assert!(!looks_like(&object[..TAIL_LEN]));
}

/// The object as a standard ZIP reader sees it: the pure-Rust `zip` crate checks each entry's
/// CRC-32 as it reads it.
#[test]
fn a_standard_zip_reader_lists_and_reads_every_entry_stored() {
    let blobs = [blob(1, 10), blob(2, 300_000), blob(3, 1)];
    let refs: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    // The second is streamed, so its header comes before its digest is known; a repeat of the
    // first is dead bytes, in no central directory entry.
    let mut writer = ObjectWriter::new(Vec::new());
    for bytes in &refs {
        let mut at = writer.begin().expect("begun");
        for part in bytes.chunks(4096) {
            writer.chunk(&mut at, part).expect("chunk");
        }
        writer.end(at, &sha256(bytes)).expect("ended");
    }
    let mut again = writer.begin().expect("begun");
    writer.chunk(&mut again, refs[0]).expect("chunk");
    writer.end(again, &sha256(refs[0])).expect("ended");
    let (object, _) = writer
        .finish(b"{\"manifest\":1}", b"{\"v\":1}")
        .expect("finished");
    let tail = read(&object).expect("read");

    let mut archive = zip::ZipArchive::new(Cursor::new(&object)).expect("a ZIP archive");
    assert_eq!(archive.len(), 3 + 2, "the dead entry is in no directory");
    let mut seen = Vec::new();
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).expect("entry");
        assert_eq!(file.compression(), zip::CompressionMethod::Stored);
        let name = file.name().expect("a name").into_owned();
        let start = file.data_start().expect("offset");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read, CRC checked");
        if name == MANIFEST_NAME {
            assert_eq!(bytes, b"{\"manifest\":1}");
        } else if name == INDEX_NAME {
            assert_eq!(bytes, b"{\"v\":1}");
        } else {
            let entry = tail
                .entries
                .iter()
                .find(|e| e.digest == sha256(&bytes))
                .expect("a blob of the object");
            // The offset this crate computes is the zip reader's own.
            assert_eq!(start, entry.offset, "{name}");
            seen.push(entry.digest);
        }
        assert_eq!(&object[start as usize..start as usize + bytes.len()], bytes);
    }
    assert_eq!(seen.len(), 3);
}

#[test]
fn every_entry_the_writer_makes_is_method_0_stored() {
    let big = blob(9, 100_000);
    let (object, _) = write(&[&blob(1, 10), &big], b"{}", 4096);
    let trailer =
        Trailer::parse(&object[object.len() - TAIL_LEN..], object.len() as u64).expect("trailer");
    // Walk the central directory: the method is the u16 at 10 of each entry.
    let mut at = trailer.cd_offset as usize;
    let mut entries = 0;
    while object[at..at + 4] == [0x50, 0x4b, 0x01, 0x02] {
        assert_eq!(u16::from_le_bytes([object[at + 10], object[at + 11]]), 0);
        let len = 46
            + usize::from(u16::from_le_bytes([object[at + 28], object[at + 29]]))
            + usize::from(u16::from_le_bytes([object[at + 30], object[at + 31]]))
            + usize::from(u16::from_le_bytes([object[at + 32], object[at + 33]]));
        at += len;
        entries += 1;
    }
    assert_eq!(entries, 4);
    // And every local header: the method is the u16 at 8.
    let mut at = 0usize;
    let mut locals = 0;
    while at < trailer.cd_offset as usize && object[at..at + 4] == [0x50, 0x4b, 0x03, 0x04] {
        assert_eq!(u16::from_le_bytes([object[at + 8], object[at + 9]]), 0);
        let name = usize::from(u16::from_le_bytes([object[at + 26], object[at + 27]]));
        let data = at + 30 + name + 20;
        locals += 1;
        // Skip header, data (found through the descriptor's signature) and descriptor.
        let mut end = data;
        while object[end..end + 4] != [0x50, 0x4b, 0x07, 0x08]
            || !is_descriptor_of(&object, data, end)
        {
            end += 1;
        }
        at = end + 24;
    }
    assert_eq!(locals, 4);
}

/// Whether the 24 bytes at `end` are the descriptor of the data that starts at `data`.
fn is_descriptor_of(object: &[u8], data: usize, end: usize) -> bool {
    let size = u64::from_le_bytes(object[end + 8..end + 16].try_into().unwrap_or([0; 8]));
    size == (end - data) as u64
}

#[test]
fn a_streamed_blob_is_the_bytes_of_a_whole_one() {
    let big = blob(9, 5000);
    let (whole, _) = write(&[&big], b"m", 0);
    let (pieces, _) = write(&[&big], b"m", 777);
    assert_eq!(whole, pieces);
}

#[test]
fn a_held_blob_is_not_written_twice() {
    let a = blob(1, 100);
    let (once, _) = write(&[&a], b"m", 0);
    let (twice, written) = write(&[&a, &a], b"m", 0);
    assert_eq!((written.blobs, written.dead_bytes), (1, 0));
    assert_eq!(once, twice);
}

#[test]
fn a_streamed_duplicate_is_dead_bytes_in_no_entry() {
    let a = blob(1, 100);
    let b = blob(2, 50);
    let (object, written) = write(&[&a, &b, &a], b"m", 33);
    assert_eq!(written.blobs, 2);
    // Header (30 + 18 + 20), the data and its descriptor (24).
    assert_eq!(written.dead_bytes, 68 + 100 + 24);
    let tail = read(&object).expect("gaps are allowed");
    assert_eq!(tail.entries.len(), 2);
    let first = tail
        .entries
        .iter()
        .find(|e| e.digest == sha256(&a))
        .expect("a");
    assert_eq!(first.offset, 68);
}

#[test]
fn the_empty_blob_is_never_a_record() {
    let empty = sha256(b"");
    let mut writer = ObjectWriter::new(Vec::new());
    assert!(!writer.blob(&empty, b"").expect("ok"));
    let at = writer.begin().expect("begun");
    assert!(!writer.end(at, &empty).expect("ok"));
    let (object, written) = writer.finish(b"m", b"").expect("finished");
    assert_eq!(written.blobs, 0);
    assert!(read(&object).expect("read").entries.is_empty());
}

#[test]
fn a_streamed_blob_of_another_length_than_its_digest_is_refused() {
    let mut writer = ObjectWriter::new(Vec::new());
    let mut at = writer.begin().expect("begun");
    writer.chunk(&mut at, b"abc").expect("chunk");
    let error = writer.end(at, &sha256(b"abcd")).expect_err("short");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn the_writer_reports_what_it_holds_and_where() {
    let mut writer = ObjectWriter::new(Vec::new());
    assert!(writer.is_empty());
    let a = blob(1, 5);
    writer.blob(&sha256(&a), &a).expect("written");
    assert_eq!(writer.len(), 68 + 5 + 24);
    assert!(writer.holds(&sha256(&a)));
    assert_eq!(writer.offset_of(&sha256(&a)), Some(68));
    assert_eq!(writer.offset_of(&sha256(b"other")), None);
    assert!(!writer.holds(&sha256(b"other")));
    let _: &mut Vec<u8> = writer.sink();
}

/// A sink that takes `room` bytes and then refuses.
struct Full {
    room: usize,
}

impl Write for Full {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.room == 0 {
            return Err(io::Error::other("full"));
        }
        let n = bytes.len().min(self.room);
        self.room -= n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_sink_that_refuses_fails_every_step_of_the_writer() {
    let a = blob(1, 8);
    let digest = sha256(&a);
    let mut writer = ObjectWriter::new(Full { room: 70 });
    assert!(writer.blob(&digest, &a).is_err());
    let mut writer = ObjectWriter::new(Full { room: 10 });
    assert!(writer.begin().is_err());
    let mut writer = ObjectWriter::new(Full { room: 68 });
    let mut at = writer.begin().expect("the header fits");
    assert!(writer.chunk(&mut at, &a).is_err());
    let mut writer = ObjectWriter::new(Full { room: 70 });
    let mut at = writer.begin().expect("the header fits");
    writer.chunk(&mut at, &a[..2]).expect("some bytes fit");
    assert!(writer.end(at, &sha256(&a[..2])).is_err());
    let writer = ObjectWriter::new(Full { room: 0 });
    assert!(writer.finish(b"m", b"").is_err());
}

/// An object with two blobs, to damage.
fn sample() -> Vec<u8> {
    write(&[&blob(1, 100), &blob(2, 40)], b"manifest", 0).0
}

/// The start of central directory entry `i` of `object`.
fn entry_at(object: &[u8], i: usize) -> usize {
    let trailer =
        Trailer::parse(&object[object.len() - TAIL_LEN..], object.len() as u64).expect("trailer");
    let mut at = trailer.cd_offset as usize;
    for _ in 0..i {
        let len = |o: usize| usize::from(u16::from_le_bytes([object[at + o], object[at + o + 1]]));
        at += 46 + len(28) + len(30) + len(32);
    }
    at
}

fn damaged(damage: impl FnOnce(&mut [u8])) -> Result<Tail, ObjectError> {
    let mut object = sample();
    damage(&mut object);
    read(&object)
}

#[test]
fn an_end_that_is_not_ours_or_does_not_add_up_is_refused() {
    assert_eq!(
        Trailer::parse(&[0; 10], 10),
        Err(ObjectError::Truncated {
            needed: TAIL_LEN as u64,
            available: 10
        })
    );
    // The end of a kbf segment (the old format) is not this one.
    let mut old = vec![0u8; 200];
    old[192..].copy_from_slice(b"KBFSEG\r\n");
    assert_eq!(read(&old), Err(ObjectError::BadMagic));
    let len = sample().len();
    // Magic, a locator field, the end record's saturated fields, and the sizes of the end record.
    for (what, at) in [
        ("magic", len - TAIL_LEN + 56),
        ("locator offset", len - TAIL_LEN + 104),
        ("end record count", len - TAIL_LEN + 124),
        ("end record size", len - TAIL_LEN + 4),
        ("end record disks", len - TAIL_LEN + 16),
        ("counts differ", len - TAIL_LEN + 24),
    ] {
        let got = damaged(|o| o[at] ^= 1);
        assert!(
            matches!(got, Err(ObjectError::BadMagic | ObjectError::BadEnd)),
            "{what}: {got:?}"
        );
    }
    for (what, at) in [
        ("manifest offset", 64),
        ("index offset", 80),
        ("cd offset", 48),
    ] {
        let got = damaged(|o| {
            let at = o.len() - TAIL_LEN + at;
            o[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        });
        assert!(
            matches!(got, Err(ObjectError::Layout { .. })),
            "{what}: {got:?}"
        );
    }
    let got = damaged(|o| {
        let at = o.len() - TAIL_LEN + 72;
        o[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    });
    assert!(matches!(got, Err(ObjectError::Layout { .. })));
    let got = damaged(|o| {
        let at = o.len() - TAIL_LEN + 32;
        o[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        o[at - 8..at].copy_from_slice(&u64::MAX.to_le_bytes());
    });
    assert!(matches!(got, Err(ObjectError::Layout { .. })));
}

#[test]
fn a_tail_of_another_length_than_the_end_says_is_truncated() {
    let object = sample();
    let trailer =
        Trailer::parse(&object[object.len() - TAIL_LEN..], object.len() as u64).expect("trailer");
    let short = &object[object.len() - trailer.tail_len() as usize + 1..];
    assert!(matches!(
        Tail::parse(&trailer, short),
        Err(ObjectError::Truncated { .. })
    ));
}

#[test]
fn an_entry_that_is_not_stored_is_refused_as_a_bad_object() {
    // Method 8 is deflate: no entry may name it, whatever its bytes.
    let object = sample();
    for i in 0..4 {
        let at = entry_at(&object, i);
        let got = damaged(|o| o[at + 10] = 8);
        assert_eq!(
            got,
            Err(ObjectError::Method {
                position: i,
                method: 8
            }),
            "{i}"
        );
    }
}

#[test]
fn damaged_members_or_entries_are_refused_for_their_own_reason() {
    let object = sample();
    let (blob_a, manifest) = (entry_at(&object, 0), entry_at(&object, 2));
    // Flags, saturated sizes, a signature and a comment.
    assert_eq!(
        damaged(|o| o[blob_a + 8] = 0).map(drop),
        Err(ObjectError::Flags { position: 0 })
    );
    assert_eq!(
        damaged(|o| o[blob_a + 20] = 0).map(drop),
        Err(ObjectError::Flags { position: 0 })
    );
    assert_eq!(
        damaged(|o| o[blob_a] = 0).map(drop),
        Err(ObjectError::Entry { position: 0 })
    );
    assert_eq!(
        damaged(|o| o[blob_a + 32] = 1).map(drop),
        Err(ObjectError::Entry { position: 0 })
    );
    // Sizes of the ZIP64 field that disagree; an unknown extra field; no ZIP64 field.
    let zip64 = |at: usize| at + 46 + 18;
    assert_eq!(
        damaged(|o| o[zip64(blob_a) + 4] ^= 1).map(drop),
        Err(ObjectError::Flags { position: 0 })
    );
    assert_eq!(
        damaged(|o| o[zip64(blob_a)] = 9).map(drop),
        Err(ObjectError::Entry { position: 0 })
    );
    // A member's CRC and its bytes.
    assert_eq!(
        damaged(|o| o[manifest + 16] ^= 1).map(drop),
        Err(ObjectError::Crc {
            name: MANIFEST_NAME
        })
    );
    // A member's name that is not a name of the format; a blob with no SHA-256 tag.
    assert!(matches!(
        damaged(|o| o[blob_a + 46] = b'x').map(drop),
        Err(ObjectError::Name { .. })
    ));
    assert!(matches!(
        damaged(|o| o[blob_a + 46 + 18 + 28] = 0).map(drop),
        Err(ObjectError::Entry { .. })
    ));
}

#[test]
fn entries_that_lie_about_their_records_are_refused() {
    let object = sample();
    let (a, b) = (entry_at(&object, 0), entry_at(&object, 1));
    let field = |entry: usize, which: usize| entry + 46 + 18 + 4 + which * 8;
    // Equal digests: the second entry's SHA-256 becomes the first's.
    let sha = |entry: usize| entry + 46 + 18 + 28 + 4;
    let got = damaged(|o| {
        let copy: Vec<u8> = o[sha(a)..sha(a) + 32].to_vec();
        o[sha(b)..sha(b) + 32].copy_from_slice(&copy);
        // The sizes must agree for the digests to be equal.
        let size: Vec<u8> = o[field(a, 0)..field(a, 0) + 8].to_vec();
        for k in 0..2 {
            o[field(b, k)..field(b, k) + 8].copy_from_slice(&size);
        }
    });
    assert!(matches!(got, Err(ObjectError::Duplicate { .. })), "{got:?}");
    // Zero size.
    let got = damaged(|o| {
        for k in 0..2 {
            o[field(a, k)..field(a, k) + 8].copy_from_slice(&0u64.to_le_bytes());
        }
    });
    assert!(matches!(got, Err(ObjectError::ZeroSize { .. })), "{got:?}");
    // Past the records, and overflowing.
    for size in [10_000u64, u64::MAX] {
        let got = damaged(|o| {
            for k in 0..2 {
                o[field(a, k)..field(a, k) + 8].copy_from_slice(&size.to_le_bytes());
            }
        });
        assert!(
            matches!(got, Err(ObjectError::OutOfBounds { .. })),
            "{size}: {got:?}"
        );
    }
    // The second blob's header moves to inside the first's bytes.
    let got = damaged(|o| {
        let first =
            u64::from_le_bytes(o[field(a, 2)..field(a, 2) + 8].try_into().unwrap_or([0; 8]));
        o[field(b, 2)..field(b, 2) + 8].copy_from_slice(&(first + 1).to_le_bytes());
    });
    assert!(matches!(got, Err(ObjectError::Overlap { .. })), "{got:?}");
    // A member moved from where the end records say.
    let manifest = entry_at(&object, 2);
    let got = damaged(|o| {
        o[field(manifest, 2)..field(manifest, 2) + 8].copy_from_slice(&0u64.to_le_bytes())
    });
    assert_eq!(
        got,
        Err(ObjectError::Member {
            name: MANIFEST_NAME
        })
    );
}

#[test]
fn a_directory_without_a_member_or_with_one_twice_or_left_over_is_refused() {
    let object = sample();
    let (manifest, index) = (entry_at(&object, 2), entry_at(&object, 3));
    // The manifest's entry renamed as the index's: the index is twice, the manifest absent.
    let got = damaged(|o| {
        let name = INDEX_NAME.as_bytes();
        let manifest_name_len = MANIFEST_NAME.len();
        assert!(name.len() <= manifest_name_len);
        o[manifest + 46..manifest + 46 + name.len()].copy_from_slice(name);
        o[manifest + 28] = name.len() as u8;
    });
    assert!(got.is_err(), "{got:?}");
    // The index's entry renamed as the manifest's.
    let got = damaged(|o| {
        let name = MANIFEST_NAME.as_bytes();
        o[index + 46..index + 46 + INDEX_NAME.len()].copy_from_slice(&name[..INDEX_NAME.len()]);
    });
    assert!(got.is_err(), "{got:?}");
    // One entry fewer than the directory holds: bytes are left over.
    let got = damaged(|o| {
        let at = o.len() - TAIL_LEN + 24;
        o[at..at + 8].copy_from_slice(&3u64.to_le_bytes());
        o[at + 8..at + 16].copy_from_slice(&3u64.to_le_bytes());
    });
    assert!(matches!(got, Err(ObjectError::Layout { .. })), "{got:?}");
}

#[test]
fn blobs_are_named_by_a_counter_and_the_members_by_fixed_names() {
    // The names are counters, and the digest is only in the extra field: two blobs, two names.
    let (object, _) = write(&[&blob(1, 10), &blob(2, 10)], b"{}", 0);
    let archive = zip::ZipArchive::new(Cursor::new(&object)).expect("zip");
    let mut names: Vec<String> = (archive.file_names())
        .map(|name| name.expect("a name").into_owned())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            INDEX_NAME,
            MANIFEST_NAME,
            "b/0000000000000001",
            "b/0000000000000002"
        ]
    );
}

#[test]
fn entries_with_a_damaged_name_extra_field_or_length_are_refused() {
    let object = sample();
    let a = entry_at(&object, 0);
    // An extra field cut to less than a header, a field of the wrong size for its tag, a name
    // longer than the directory, and a directory that holds fewer entries than it says.
    for (what, damage) in [
        (
            "short extra",
            Box::new(move |o: &mut [u8]| o[a + 30] = 2) as Box<dyn Fn(&mut [u8])>,
        ),
        (
            "zip64 size",
            Box::new(move |o: &mut [u8]| o[a + 46 + 18 + 2] = 16),
        ),
        (
            "sha size",
            Box::new(move |o: &mut [u8]| o[a + 46 + 18 + 28 + 2] = 16),
        ),
        ("long name", Box::new(move |o: &mut [u8]| o[a + 29] = 0xFF)),
    ] {
        let got = damaged(|o| damage(o));
        assert_eq!(
            got.map(drop),
            Err(ObjectError::Entry { position: 0 }),
            "{what}"
        );
    }
    let got = damaged(|o| {
        let at = o.len() - TAIL_LEN + 24;
        // Six entries are claimed (and fit the directory's length) where four are held.
        o[at..at + 8].copy_from_slice(&6u64.to_le_bytes());
        o[at + 8..at + 16].copy_from_slice(&6u64.to_le_bytes());
    });
    assert_eq!(got.map(drop), Err(ObjectError::Entry { position: 4 }));
}

#[test]
fn a_blob_entry_with_no_sha_256_or_a_member_twice_is_refused() {
    let object = sample();
    let (a, manifest) = (entry_at(&object, 0), entry_at(&object, 2));
    // The manifest's entry renamed to a blob's name has no SHA-256 tag.
    let got =
        damaged(|o| o[manifest + 46..manifest + 46 + 18].copy_from_slice(b"b/0000000000000009"));
    assert_eq!(got.map(drop), Err(ObjectError::Entry { position: 2 }));
    // A blob's entry renamed to the manifest's: the manifest twice (the second is not where
    // the end records say).
    let got = damaged(|o| o[a + 46..a + 46 + 18].copy_from_slice(MANIFEST_NAME.as_bytes()));
    assert_eq!(
        got.map(drop),
        Err(ObjectError::Member {
            name: MANIFEST_NAME
        })
    );
    // The manifest's entry renamed to a blob's name AND the blob's SHA-256 tag kept: no member
    // is left, which the directory's end says.
    let got =
        damaged(|o| o[manifest + 46..manifest + 46 + 18].copy_from_slice(b"b/0000000000000009"));
    assert!(got.is_err());
}

#[test]
fn looks_like_refuses_what_is_not_an_end() {
    assert!(!looks_like(&[]));
    assert!(!looks_like(&[0; TAIL_LEN]));
}
