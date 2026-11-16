//! The one-pass task object: one ZIP archive per action, written in one pass over the outputs
//! (docs/design/one-pass-task-object.md).
//!
//! The archive is **uncompressed**: every entry is method 0 (stored), so each file's bytes are
//! contiguous at its offset, as in tar. There is no compression code path here, and a reader
//! refuses an entry of any other method. ZIP64 is used throughout, so no size or offset is
//! limited to 32 bits.
//!
//! ```text
//! entry*     local header (30 + name + 20 bytes, flag bit 3, sizes and CRC-32 zero)
//!            data (the bytes, stored)
//!            data descriptor (24 bytes: signature, CRC-32, compressed size u64, size u64)
//! entry      .kbf/manifest.json   the manifest JSON
//! entry      .kbf/index.json      the neutral file index (JSON, may be empty)
//! central directory               one entry per distinct blob and the two members
//! ZIP64 end of central directory  with a private extension (below)
//! ZIP64 locator
//! end of central directory        22 bytes, no comment
//! ```
//!
//! Because a blob's digest is known only once its last byte is read, a blob is named by a
//! counter (`b/<16 hex digits>`), not by its digest: the local header comes first. Its SHA-256
//! is carried in the central directory, in an extra field with the private header id
//! [`SHA256_TAG`] (0x4B42, "KB"; not a registered id, ignored by every reader that does not know
//! it) of 32 bytes. A blob whose digest was already written is *dead*: its entry is in no
//! central directory and its bytes stay in the object. Every local header is exactly
//! `30 + name + 20` bytes (the 20 being the ZIP64 extra field), so a blob's bytes start at its
//! header's offset plus that, which a reader may compute from the central directory alone.
//!
//! The ZIP64 end record's extensible data is this format's trailer, so a reader finds the manifest
//! and the index from the last [`TAIL_LEN`] bytes of the object without scanning:
//! `magic [8] | manifest_offset u64 | manifest_len u64 | index_offset u64 | index_len u64`,
//! the offsets being those of the members' *data*. One ranged read from `manifest_offset` to the
//! end then holds the manifest, the index and the central directory.
//!
//! [`ObjectWriter`] writes through any [`Write`]. [`Trailer::parse`] and [`Tail::parse`] are the
//! reader. Neither reads a blob.

use std::collections::BTreeMap;
use std::io::{self, Write};

use crc32fast::Hasher as Crc32;
use kbf_types::{Digest, DigestFunction};
use sha2::{Digest as _, Sha256};

/// The private extra-field header id carrying a blob's SHA-256 in its central directory entry.
pub const SHA256_TAG: u16 = 0x4B42;

/// The first eight bytes of the ZIP64 end record's extensible data.
pub const MAGIC: [u8; 8] = *b"KBFTASK1";

/// The name of the member that holds the manifest.
pub const MANIFEST_NAME: &str = ".kbf/manifest.json";

/// The name of the member that holds the neutral file index.
pub const INDEX_NAME: &str = ".kbf/index.json";

/// The length of the end of every object: the end of central directory (22), the ZIP64 locator
/// (20) and the ZIP64 end record (56 and the 40 bytes of extensible data).
pub const TAIL_LEN: usize = 22 + 20 + 56 + 40;

/// The length of a blob's name, `b/` and 16 hex digits: fixed, so that a blob's offset is
/// computable from the central directory.
const BLOB_NAME_LEN: usize = 18;
const LOCAL_FIXED: u64 = 30;
const LOCAL_EXTRA: u64 = 20;
const DESCRIPTOR: u64 = 24;
const SIG_LOCAL: u32 = 0x0403_4b50;
const SIG_DESCRIPTOR: u32 = 0x0807_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;
const SIG_EOCD64: u32 = 0x0606_4b50;
const SIG_LOCATOR: u32 = 0x0706_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;
const ZIP64_TAG: u16 = 1;
/// General purpose flag bit 3: sizes and CRC-32 follow the data.
const FLAG_DESCRIPTOR: u16 = 0x0008;
/// Method 0: stored. The only method there is.
const METHOD_STORED: u16 = 0;
/// 1980-01-01, the earliest date a ZIP can hold: entries carry no time.
const DOS_DATE: u16 = 0x0021;
const VERSION: u16 = 45;

/// Why an object is not a well-formed one-pass object.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ObjectError {
    /// Fewer bytes than the end of the object, or than the end says follow the records.
    #[error("one-pass object truncated: needed {needed} bytes, had {available}")]
    Truncated {
        /// Bytes needed.
        needed: u64,
        /// Bytes supplied.
        available: u64,
    },
    /// The end is not the end of a one-pass object: a signature or the magic is wrong.
    #[error("not a one-pass object: wrong signature or magic")]
    BadMagic,
    /// A field of the end records that must be zero or a fixed value is not.
    #[error("one-pass end records have an unexpected field")]
    BadEnd,
    /// The offsets of the end records do not add up to the object's length.
    #[error("one-pass offsets do not describe an object of {actual} bytes")]
    Layout {
        /// The object's length.
        actual: u64,
    },
    /// A central directory entry is damaged: a signature, a length or an extra field.
    #[error("the central directory entry at position {position} is damaged")]
    Entry {
        /// The entry's position.
        position: usize,
    },
    /// An entry that is not stored (method 0): this format has no compression.
    #[error("the entry at position {position} has method {method}, not 0 (stored)")]
    Method {
        /// The entry's position.
        position: usize,
        /// The method it names.
        method: u16,
    },
    /// An entry whose sizes disagree, or whose flags are not those of this format.
    #[error("the entry at position {position} is not a stored entry with a data descriptor")]
    Flags {
        /// The entry's position.
        position: usize,
    },
    /// An entry with a name that is neither a blob's nor a member's, or a member twice or absent.
    #[error("the central directory names {name:?}, which this format does not")]
    Name {
        /// The name.
        name: String,
    },
    /// A member's CRC-32 does not match its bytes.
    #[error("the CRC-32 of {name} does not match its bytes")]
    Crc {
        /// The member.
        name: &'static str,
    },
    /// A member is not where the end records say.
    #[error("the member {name} is not where the end records say")]
    Member {
        /// The member.
        name: &'static str,
    },
    /// Two entries share a digest.
    #[error("two entries hold the blob {digest}")]
    Duplicate {
        /// The digest.
        digest: Digest,
    },
    /// A record of size 0 (the empty blob is implicit in every store and is never a record).
    #[error("the record at offset {offset} has size 0")]
    ZeroSize {
        /// Where the record starts.
        offset: u64,
    },
    /// A record that is not inside the records.
    #[error("the record {digest} lies outside the records")]
    OutOfBounds {
        /// The record's digest.
        digest: Digest,
    },
    /// Two records share bytes.
    #[error("the records {first} and {second} overlap")]
    Overlap {
        /// The record that starts first.
        first: Digest,
        /// The record that starts inside it.
        second: Digest,
    },
}

/// A blob of the object: its digest, where its bytes start, and the CRC-32 of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The blob's SHA-256 digest; its size is the record's length.
    pub digest: Digest,
    /// Where the blob's bytes start.
    pub offset: u64,
    /// The CRC-32 (ZIP's, IEEE) of the bytes.
    pub crc32: u32,
}

impl Entry {
    /// The record's byte range in the object.
    #[must_use]
    pub const fn range(&self) -> std::ops::Range<u64> {
        self.offset..self.offset + self.digest.size_bytes
    }

    /// Checks that `record` is this entry's blob: its length, its CRC-32, then its digest.
    #[must_use]
    pub fn verify(&self, record: &[u8]) -> bool {
        let digest = Digest::new(
            DigestFunction::Sha256,
            Sha256::digest(record).into(),
            record.len() as u64,
        );
        [
            record.len() as u64 == self.digest.size_bytes,
            crc32(record) == self.crc32,
            digest == self.digest,
        ]
        .iter()
        .all(|ok| *ok)
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut hasher = Crc32::new();
    hasher.update(bytes);
    hasher.finalize()
}

/// Where the manifest and the index are, from the end of the object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trailer {
    /// Where the manifest member's data starts.
    pub manifest_offset: u64,
    /// The manifest's length.
    pub manifest_len: u64,
    /// Where the index member's data starts.
    pub index_offset: u64,
    /// The index's length.
    pub index_len: u64,
    /// Where the central directory starts.
    pub cd_offset: u64,
    /// How many entries the central directory holds (the two members included).
    pub entry_count: u64,
    object_len: u64,
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    let raw = bytes.get(at..at + 2).and_then(|b| b.try_into().ok());
    raw.map_or(0, u16::from_le_bytes)
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let raw = bytes.get(at..at + 4).and_then(|b| b.try_into().ok());
    raw.map_or(0, u32::from_le_bytes)
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let raw = bytes.get(at..at + 8).and_then(|b| b.try_into().ok());
    raw.map_or(0, u64::from_le_bytes)
}

/// The error of the first condition that holds, if any: every condition is evaluated (none
/// indexes past a slice, the readers above give 0 for what is not there), so a list of checks
/// is one place, not one branch each.
fn first_of(checks: &[(bool, ObjectError)]) -> Result<(), ObjectError> {
    match checks.iter().find(|(wrong, _)| *wrong) {
        Some((_, error)) => Err(error.clone()),
        None => Ok(()),
    }
}

/// Whether `end`, the last bytes of an object, is the end of a one-pass object: the three end
/// records' signatures and the magic.
#[must_use]
pub fn looks_like(end: &[u8]) -> bool {
    [
        end.len() == TAIL_LEN,
        u32_at(end, 0) == SIG_EOCD64,
        u32_at(end, 96) == SIG_LOCATOR,
        u32_at(end, 116) == SIG_EOCD,
        end.get(56..64) == Some(&MAGIC[..]),
    ]
    .iter()
    .all(|ok| *ok)
}

impl Trailer {
    /// Parses `end`, the last [`TAIL_LEN`] bytes of an object of `object_len` bytes, and checks
    /// that its offsets describe that object.
    ///
    /// # Errors
    /// [`ObjectError`] for the first rule that fails.
    pub fn parse(end: &[u8], object_len: u64) -> Result<Self, ObjectError> {
        let truncated = ObjectError::Truncated {
            needed: TAIL_LEN as u64,
            available: end.len() as u64,
        };
        let layout = ObjectError::Layout { actual: object_len };
        let eocd64_at = object_len.saturating_sub(TAIL_LEN as u64);
        let e = 116;
        first_of(&[
            (end.len() != TAIL_LEN, truncated),
            (!looks_like(end), ObjectError::BadMagic),
            // ZIP64 end record: size of the rest (44 + 40), versions, disks, counts.
            (
                [
                    u64_at(end, 4) != 44 + 40,
                    u32_at(end, 16) != 0,
                    u32_at(end, 20) != 0,
                    u64_at(end, 24) != u64_at(end, 32),
                    // The locator: disk 0, the end record's offset, one disk.
                    u32_at(end, 100) != 0,
                    u64_at(end, 104) != eocd64_at,
                    u32_at(end, 112) != 1,
                    // The plain end record: no disks, saturated counts and offsets, no comment.
                    u16_at(end, e + 4) != 0,
                    u16_at(end, e + 6) != 0,
                    u16_at(end, e + 8) != 0xFFFF,
                    u16_at(end, e + 10) != 0xFFFF,
                    u32_at(end, e + 12) != u32::MAX,
                    u32_at(end, e + 16) != u32::MAX,
                    u16_at(end, e + 20) != 0,
                    // A record's end is not before the object's start.
                    object_len < TAIL_LEN as u64,
                ]
                .iter()
                .any(|wrong| *wrong),
                ObjectError::BadEnd,
            ),
        ])?;
        let entry_count = u64_at(end, 32);
        let cd_size = u64_at(end, 40);
        let cd_offset = u64_at(end, 48);
        let parsed = Self {
            manifest_offset: u64_at(end, 64),
            manifest_len: u64_at(end, 72),
            index_offset: u64_at(end, 80),
            index_len: u64_at(end, 88),
            cd_offset,
            entry_count,
            object_len,
        };
        let manifest_end = parsed.manifest_offset.checked_add(parsed.manifest_len);
        let index_end = parsed.index_offset.checked_add(parsed.index_len);
        // The central directory follows the last record and ends where the end records start;
        // the index member follows the manifest member; each entry takes 46 bytes at least,
        // which bounds the count by the directory.
        first_of(&[(
            [
                manifest_end.is_none_or(|end| end > parsed.index_offset),
                index_end.is_none_or(|end| end > cd_offset),
                cd_offset.checked_add(cd_size) != Some(eocd64_at),
                entry_count
                    .checked_mul(46)
                    .is_none_or(|least| least > cd_size),
            ]
            .iter()
            .any(|wrong| *wrong),
            layout,
        )])?;
        Ok(parsed)
    }

    /// How many bytes from the end of the object a reader fetches next: the manifest, the index
    /// and the central directory with the end records.
    #[must_use]
    pub const fn tail_len(&self) -> u64 {
        self.object_len - self.manifest_offset
    }
}

/// The manifest, the index and the blobs of a one-pass object, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tail {
    /// The manifest member's bytes.
    pub manifest: Vec<u8>,
    /// The neutral file index (docs/design/one-pass-task-object.md section 3). Opaque here;
    /// empty for none.
    pub files: Vec<u8>,
    /// Every blob, in digest order.
    pub entries: Vec<Entry>,
}

/// One central directory entry, as far as this format reads it.
struct Central<'a> {
    name: &'a [u8],
    crc32: u32,
    size: u64,
    header_offset: u64,
    sha256: Option<[u8; 32]>,
}

/// Reads the central directory entry at the start of `bytes`; its length too.
fn central(bytes: &[u8], position: usize) -> Result<(Central<'_>, usize), ObjectError> {
    let bad = ObjectError::Entry { position };
    let method = u16_at(bytes, 10);
    let (name_len, extra_len, comment_len) = (
        usize::from(u16_at(bytes, 28)),
        usize::from(u16_at(bytes, 30)),
        usize::from(u16_at(bytes, 32)),
    );
    let len = 46 + name_len + extra_len + comment_len;
    let saturated = [
        u32_at(bytes, 20) == u32::MAX,
        u32_at(bytes, 24) == u32::MAX,
        u32_at(bytes, 42) == u32::MAX,
    ];
    first_of(&[
        (
            [
                bytes.len() < 46,
                u32_at(bytes, 0) != SIG_CENTRAL,
                bytes.len() < len,
                comment_len != 0,
            ]
            .iter()
            .any(|wrong| *wrong),
            bad.clone(),
        ),
        (
            method != METHOD_STORED,
            ObjectError::Method { position, method },
        ),
        (
            u16_at(bytes, 8) != FLAG_DESCRIPTOR || !saturated.iter().all(|ok| *ok),
            ObjectError::Flags { position },
        ),
    ])?;
    let name = &bytes[46..46 + name_len];
    let mut extra = &bytes[46 + name_len..46 + name_len + extra_len];
    let (mut zip64, mut sha256) = (None, None);
    while !extra.is_empty() {
        let (tag, size) = (u16_at(extra, 0), usize::from(u16_at(extra, 2)));
        match (tag, size, extra.get(4..4 + size)) {
            (ZIP64_TAG, 24, Some(data)) => {
                zip64 = Some((u64_at(data, 0), u64_at(data, 8), u64_at(data, 16)));
            }
            (SHA256_TAG, 32, Some(data)) => sha256 = data.try_into().ok(),
            _ => return Err(bad),
        }
        extra = &extra[4 + size..];
    }
    let (size, compressed, header_offset) = zip64.unwrap_or_default();
    first_of(&[
        (zip64.is_none(), bad),
        (size != compressed, ObjectError::Flags { position }),
    ])?;
    let entry = Central {
        name,
        crc32: u32_at(bytes, 16),
        size,
        header_offset,
        sha256,
    };
    Ok((entry, len))
}

impl Tail {
    /// Parses `tail`, the last [`Trailer::tail_len`] bytes of the object `trailer` is the end of.
    ///
    /// Checks: both members, by name, CRC-32 and place; that every other entry is a stored
    /// blob (method 0, never any other) with a SHA-256, none twice and none of size 0; that each
    /// lies inside the records (before the manifest) and no two overlap. Gaps between records
    /// are allowed. Reads no blob.
    ///
    /// # Errors
    /// [`ObjectError`] for the first rule that fails.
    pub fn parse(trailer: &Trailer, tail: &[u8]) -> Result<Self, ObjectError> {
        let layout = ObjectError::Layout {
            actual: trailer.object_len,
        };
        first_of(&[(
            (tail.len() as u64) != trailer.tail_len(),
            ObjectError::Truncated {
                needed: trailer.tail_len(),
                available: tail.len() as u64,
            },
        )])?;
        // `tail` is as long as the end of an object a reader holds, so these fit `usize`.
        let at =
            |offset: u64| usize::try_from(offset - trailer.manifest_offset).unwrap_or(usize::MAX);
        let slice = |offset: u64, len: u64| {
            let start = at(offset);
            let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX));
            tail.get(start..end)
        };
        let (manifest, files) = (
            slice(trailer.manifest_offset, trailer.manifest_len),
            slice(trailer.index_offset, trailer.index_len),
        );
        let directory = tail.get(at(trailer.cd_offset)..tail.len() - TAIL_LEN);
        first_of(&[
            (
                manifest.is_none(),
                ObjectError::Member {
                    name: MANIFEST_NAME,
                },
            ),
            (files.is_none(), ObjectError::Member { name: INDEX_NAME }),
            (directory.is_none(), layout.clone()),
        ])?;
        let members = [
            (
                MANIFEST_NAME,
                manifest.unwrap_or_default(),
                trailer.manifest_offset,
            ),
            (INDEX_NAME, files.unwrap_or_default(), trailer.index_offset),
        ];
        let mut rest = directory.unwrap_or_default();
        let mut seen = [0u8; 2];
        let mut entries: Vec<Entry> = Vec::new();
        for position in 0..usize::try_from(trailer.entry_count).unwrap_or(usize::MAX) {
            let (entry, len) = central(rest, position)?;
            rest = &rest[len..];
            let data = entry.header_offset + LOCAL_FIXED + entry.name.len() as u64 + LOCAL_EXTRA;
            let member = members.iter().position(|m| m.0.as_bytes() == entry.name);
            if let Some(i) = member {
                let (name, bytes, offset) = members[i];
                seen[i] = seen[i].saturating_add(1);
                first_of(&[
                    (
                        data != offset || entry.size != bytes.len() as u64,
                        ObjectError::Member { name },
                    ),
                    (crc32(bytes) != entry.crc32, ObjectError::Crc { name }),
                ])?;
                continue;
            }
            let digest = Digest::new(
                DigestFunction::Sha256,
                entry.sha256.unwrap_or_default(),
                entry.size,
            );
            let end = data.checked_add(entry.size);
            first_of(&[
                (
                    entry.name.len() != BLOB_NAME_LEN || !entry.name.starts_with(b"b/"),
                    ObjectError::Name {
                        name: String::from_utf8_lossy(entry.name).into_owned(),
                    },
                ),
                (entry.sha256.is_none(), ObjectError::Entry { position }),
                (entry.size == 0, ObjectError::ZeroSize { offset: data }),
                (
                    end.is_none_or(|end| end > trailer.manifest_offset),
                    ObjectError::OutOfBounds { digest },
                ),
            ])?;
            entries.push(Entry {
                digest,
                offset: data,
                crc32: entry.crc32,
            });
        }
        first_of(&[
            (!rest.is_empty(), layout),
            (
                seen != [1, 1],
                ObjectError::Name {
                    name: "a member of the central directory is absent or twice".to_owned(),
                },
            ),
        ])?;
        entries.sort_by_key(|e| e.digest);
        let twice = entries.windows(2).find(|p| p[0].digest == p[1].digest);
        first_of(&[(
            twice.is_some(),
            ObjectError::Duplicate {
                digest: twice.map_or(Digest::new(DigestFunction::Sha256, [0; 32], 0), |p| {
                    p[0].digest
                }),
            },
        )])?;
        let mut by_offset: Vec<&Entry> = entries.iter().collect();
        by_offset.sort_by_key(|e| e.offset);
        let overlap = by_offset
            .windows(2)
            .find(|p| p[1].offset < p[0].range().end);
        if let Some(pair) = overlap {
            return Err(ObjectError::Overlap {
                first: pair[0].digest,
                second: pair[1].digest,
            });
        }
        Ok(Self {
            manifest: members[0].1.to_vec(),
            files: members[1].1.to_vec(),
            entries,
        })
    }
}

/// A blob being streamed into the object: its bytes are written as they come, and it is listed
/// only by [`ObjectWriter::end`], once its digest is known.
#[derive(Debug)]
pub struct Streaming {
    /// Where its local header starts.
    header: u64,
    crc: Crc32,
    len: u64,
    /// The bytes of the whole entry so far: header and data.
    written: u64,
    name: String,
}

/// What a finished object is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    /// The SHA-256 digest of the whole object, hashed as it was written.
    pub object: Digest,
    /// How many distinct blobs the central directory holds, the two members not counted.
    pub blobs: u64,
    /// The bytes of entries (header, data, descriptor) of a streamed blob that turned out to be a
    /// duplicate of an earlier one: in the object, in no central directory entry.
    pub dead_bytes: u64,
}

/// One entry of the central directory to come.
struct Listed {
    name: String,
    header: u64,
    size: u64,
    crc32: u32,
    sha256: Option<[u8; 32]>,
}

impl Listed {
    /// Where the entry's data starts.
    fn data(&self) -> u64 {
        self.header + LOCAL_FIXED + self.name.len() as u64 + LOCAL_EXTRA
    }
}

/// Writes a one-pass object through `W`, one blob at a time; see the module documentation.
///
/// The writer holds the central directory's entries (one per distinct blob) and nothing of a
/// blob. It is single-writer: a streamed blob ([`ObjectWriter::begin`]) is ended before the next
/// blob starts.
pub struct ObjectWriter<W: Write> {
    out: W,
    len: u64,
    object: Sha256,
    listed: Vec<Listed>,
    /// A digest's place in `listed`.
    held: BTreeMap<Digest, usize>,
    next: u64,
    dead: u64,
}

impl<W: Write> ObjectWriter<W> {
    /// A writer at the start of an empty `out`.
    pub fn new(out: W) -> Self {
        Self {
            out,
            len: 0,
            object: Sha256::new(),
            listed: Vec::new(),
            held: BTreeMap::new(),
            next: 0,
            dead: 0,
        }
    }

    /// Whether a blob of this digest is in the central directory to come.
    #[must_use]
    pub fn holds(&self, digest: &Digest) -> bool {
        self.held.contains_key(digest)
    }

    /// Where the bytes of the blob `digest` start in the object, if it is held.
    #[must_use]
    pub fn offset_of(&self, digest: &Digest) -> Option<u64> {
        self.held.get(digest).map(|at| self.listed[*at].data())
    }

    /// The sink, for a caller that must flush it before it reads what was written.
    pub fn sink(&mut self) -> &mut W {
        &mut self.out
    }

    /// The bytes written so far.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Whether nothing has been written.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.object.update(bytes);
        self.len += bytes.len() as u64;
        Ok(())
    }

    /// Writes the local header of an entry named `name`; returns its offset.
    fn header(&mut self, name: &str) -> io::Result<u64> {
        let at = self.len;
        let mut head = Vec::with_capacity(50 + name.len());
        head.extend_from_slice(&SIG_LOCAL.to_le_bytes());
        head.extend_from_slice(&VERSION.to_le_bytes());
        head.extend_from_slice(&FLAG_DESCRIPTOR.to_le_bytes());
        head.extend_from_slice(&METHOD_STORED.to_le_bytes());
        head.extend_from_slice(&0u16.to_le_bytes()); // time
        head.extend_from_slice(&DOS_DATE.to_le_bytes());
        head.extend_from_slice(&[0; 12]); // CRC-32 and sizes: they follow the data
        head.extend_from_slice(&(name.len() as u16).to_le_bytes());
        head.extend_from_slice(&(LOCAL_EXTRA as u16).to_le_bytes());
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(&ZIP64_TAG.to_le_bytes());
        head.extend_from_slice(&16u16.to_le_bytes());
        head.extend_from_slice(&[0; 16]);
        self.put(&head)?;
        Ok(at)
    }

    /// Writes the data descriptor of an entry whose data is `size` bytes and CRC-32 `crc32`.
    fn descriptor(&mut self, crc32: u32, size: u64) -> io::Result<()> {
        let mut tail = Vec::with_capacity(DESCRIPTOR as usize);
        tail.extend_from_slice(&SIG_DESCRIPTOR.to_le_bytes());
        tail.extend_from_slice(&crc32.to_le_bytes());
        tail.extend_from_slice(&size.to_le_bytes());
        tail.extend_from_slice(&size.to_le_bytes());
        self.put(&tail)
    }

    fn blob_name(&mut self) -> String {
        self.next += 1;
        format!("b/{:016x}", self.next)
    }

    /// Writes a whole entry named `name` of `bytes`; returns the index of its listing.
    fn entry(&mut self, name: String, bytes: &[u8], sha256: Option<[u8; 32]>) -> io::Result<usize> {
        let header = self.header(&name)?;
        self.put(bytes)?;
        let crc32 = crc32(bytes);
        self.descriptor(crc32, bytes.len() as u64)?;
        self.listed.push(Listed {
            name,
            header,
            size: bytes.len() as u64,
            crc32,
            sha256,
        });
        Ok(self.listed.len() - 1)
    }

    /// Writes `bytes`, the blob `digest` names, unless the empty blob or one already written.
    /// Returns whether it was written. Nothing is written for a blob that is not new, so a blob
    /// held in memory never costs a dead byte.
    ///
    /// # Errors
    /// The sink's error.
    pub fn blob(&mut self, digest: &Digest, bytes: &[u8]) -> io::Result<bool> {
        if digest.size_bytes == 0 || self.holds(digest) {
            return Ok(false);
        }
        let name = self.blob_name();
        let at = self.entry(name, bytes, Some(digest.hash))?;
        self.held.insert(*digest, at);
        Ok(true)
    }

    /// Starts a blob whose digest is not yet known: its local header is written now, its bytes
    /// follow in [`ObjectWriter::chunk`] calls and [`ObjectWriter::end`] names it.
    ///
    /// # Errors
    /// The sink's error.
    pub fn begin(&mut self) -> io::Result<Streaming> {
        let name = self.blob_name();
        let before = self.len;
        let header = self.header(&name)?;
        Ok(Streaming {
            header,
            crc: Crc32::new(),
            len: 0,
            written: self.len - before,
            name,
        })
    }

    /// Writes the next bytes of the streamed blob `at`.
    ///
    /// # Errors
    /// The sink's error.
    pub fn chunk(&mut self, at: &mut Streaming, bytes: &[u8]) -> io::Result<()> {
        self.put(bytes)?;
        at.crc.update(bytes);
        at.len += bytes.len() as u64;
        at.written += bytes.len() as u64;
        Ok(())
    }

    /// Ends the streamed blob `at`, whose bytes hash to `digest`: writes its data descriptor. A
    /// new blob is listed where it began. One already held (or empty) is not listed and its
    /// bytes are dead: they stay in the object. Returns whether it was listed.
    ///
    /// # Errors
    /// `InvalidData` when the bytes written are not `digest.size_bytes` long; the sink's error.
    pub fn end(&mut self, at: Streaming, digest: &Digest) -> io::Result<bool> {
        if at.len != digest.size_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a streamed blob is not as long as its digest says",
            ));
        }
        let crc32 = at.crc.finalize();
        self.descriptor(crc32, at.len)?;
        if digest.size_bytes == 0 || self.holds(digest) {
            self.dead += at.written + DESCRIPTOR;
            return Ok(false);
        }
        self.listed.push(Listed {
            name: at.name,
            header: at.header,
            size: at.len,
            crc32,
            sha256: Some(digest.hash),
        });
        self.held.insert(*digest, self.listed.len() - 1);
        Ok(true)
    }

    /// Ends the object: writes the manifest and the index as members, the central directory and
    /// the end records, and returns the sink with what was written. The sink is not flushed.
    ///
    /// # Errors
    /// The sink's error.
    pub fn finish(mut self, manifest: &[u8], files: &[u8]) -> io::Result<(W, Written)> {
        let blobs = self.held.len() as u64;
        let manifest_at = self.entry(MANIFEST_NAME.to_owned(), manifest, None)?;
        let index_at = self.entry(INDEX_NAME.to_owned(), files, None)?;
        let manifest_offset = self.listed[manifest_at].data();
        let index_offset = self.listed[index_at].data();
        let cd_offset = self.len;
        let mut directory = Vec::new();
        for entry in &self.listed {
            central_entry(&mut directory, entry);
        }
        let cd_size = directory.len() as u64;
        self.put(&directory)?;
        let count = self.listed.len() as u64;
        let mut end = Vec::with_capacity(TAIL_LEN);
        // ZIP64 end of central directory record, then its private extensible data.
        end.extend_from_slice(&SIG_EOCD64.to_le_bytes());
        end.extend_from_slice(&(44u64 + 40).to_le_bytes());
        end.extend_from_slice(&VERSION.to_le_bytes());
        end.extend_from_slice(&VERSION.to_le_bytes());
        end.extend_from_slice(&[0; 8]); // this disk, the disk of the directory
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&count.to_le_bytes());
        end.extend_from_slice(&cd_size.to_le_bytes());
        end.extend_from_slice(&cd_offset.to_le_bytes());
        end.extend_from_slice(&MAGIC);
        let members = [
            manifest_offset,
            manifest.len() as u64,
            index_offset,
            files.len() as u64,
        ];
        for n in members {
            end.extend_from_slice(&n.to_le_bytes());
        }
        end.extend_from_slice(&SIG_LOCATOR.to_le_bytes());
        end.extend_from_slice(&0u32.to_le_bytes());
        end.extend_from_slice(&(cd_offset + cd_size).to_le_bytes());
        end.extend_from_slice(&1u32.to_le_bytes());
        end.extend_from_slice(&SIG_EOCD.to_le_bytes());
        end.extend_from_slice(&[0; 4]); // disks
        end.extend_from_slice(&0xFFFFu16.to_le_bytes());
        end.extend_from_slice(&0xFFFFu16.to_le_bytes());
        end.extend_from_slice(&u32::MAX.to_le_bytes());
        end.extend_from_slice(&u32::MAX.to_le_bytes());
        end.extend_from_slice(&0u16.to_le_bytes()); // no comment
        self.put(&end)?;
        let hash = self.object.finalize().into();
        let written = Written {
            object: Digest::new(DigestFunction::Sha256, hash, self.len),
            blobs,
            dead_bytes: self.dead,
        };
        Ok((self.out, written))
    }
}

/// Appends the central directory entry of `entry` to `out`.
fn central_entry(out: &mut Vec<u8>, entry: &Listed) {
    let extra_len = 4 + 24 + if entry.sha256.is_some() { 4 + 32 } else { 0 };
    out.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
    out.extend_from_slice(&((3u16 << 8) | VERSION).to_le_bytes()); // made by: Unix
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&FLAG_DESCRIPTOR.to_le_bytes());
    out.extend_from_slice(&METHOD_STORED.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // time
    out.extend_from_slice(&DOS_DATE.to_le_bytes());
    out.extend_from_slice(&entry.crc32.to_le_bytes());
    out.extend_from_slice(&u32::MAX.to_le_bytes()); // sizes are in the ZIP64 field
    out.extend_from_slice(&u32::MAX.to_le_bytes());
    out.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
    out.extend_from_slice(&(extra_len as u16).to_le_bytes());
    out.extend_from_slice(&[0; 2]); // comment length
    out.extend_from_slice(&[0; 2]); // disk
    out.extend_from_slice(&[0; 2]); // internal attributes
    out.extend_from_slice(&(0o100_644u32 << 16).to_le_bytes());
    out.extend_from_slice(&u32::MAX.to_le_bytes()); // the offset is in the ZIP64 field
    out.extend_from_slice(entry.name.as_bytes());
    out.extend_from_slice(&ZIP64_TAG.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&entry.size.to_le_bytes());
    out.extend_from_slice(&entry.size.to_le_bytes());
    out.extend_from_slice(&entry.header.to_le_bytes());
    if let Some(sha256) = entry.sha256 {
        out.extend_from_slice(&SHA256_TAG.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&sha256);
    }
}
