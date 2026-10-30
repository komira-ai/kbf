//! The manifest of a task pack: the JSON record at offset 0 in which a task states what its
//! action did.
//!
//! The task writes it and the frontend reads it, so this one type is both ends: [`TaskManifest::parse`]
//! is the reader and [`TaskManifest::to_json`] the writer, and the canonical bytes are the
//! compact JSON of the wire form below, fields in this order. Nothing in a manifest is trusted:
//! the frontend hashes every record, rebuilds the action result from its own hashing and the
//! lease row, and takes the exit code and the output list as the writer's claim. What this
//! module checks is the shape alone, the same on both ends.
//!
//! The wire form is
//!
//! ```json
//! {"version":1,"lease":"<build id>","action":"<hex>/<size>","exit_code":0,
//!  "stdout":"<hex>/<size>","stderr":"<hex>/<size>",
//!  "output_files":[{"path":"out/a.o","digest":"<hex>/<size>","is_executable":false}],
//!  "output_directories":[{"path":"out/d","tree":"<hex>/<size>"}],
//!  "output_symlinks":[{"path":"out/l","target":"a.o"}],
//!  "timing":{"fetch_ms":0,"run_ms":0,"pack_ms":0},
//!  "usage":{"cpu_usage_us":0,"memory_peak_bytes":0}}
//! ```
//!
//! with every field required except `stdout` and `stderr`, which are left out when the stream
//! is empty. An unknown field, a duplicate key and uppercase hex are refused.
//!
//! Version 2 is for a run that wrote several packs or large objects (a blob over
//! [`MAX_RECORD_BYTES`] is a raw object of its own). It adds two required fields after `usage`,
//! which version 1 must not have:
//!
//! ```json
//!  ,"packs":2,"large":[{"slot":0,"digest":"<hex>/<size>"}]
//! ```
//!
//! and may add an advisory `"inputs":{..}` ([`InputStats`]) after them, which only version 2
//! may carry: a manifest with `inputs` is written as version 2, since a version 1 reader
//! refuses a field it does not know. Version 2 is required exactly when `packs` is above 1, `large`
//! is not empty or `inputs` is present: a body of either version that disagrees is refused, so a
//! manifest has one spelling.
//!
//! `packs` is how many of packs `0..` were written (1 to [`MAX_PACKS`]); `large` names the
//! large objects by slot (unique, below [`MAX_LARGE`]), each of a size above
//! [`MAX_RECORD_BYTES`] and at most the large cap the reader is given. Every digest an output
//! names is in exactly one place, a pack record or one large object; which pack holds it is for
//! ingest to check. A manifest of one pack and no large object is written as version 1, so the
//! bytes of a small run do not change.

use std::collections::BTreeSet;

use kbf_types::{Digest, DigestFunction, ParseDigestError};
use serde::{Deserialize, Serialize};

use crate::task_key::{MAX_LARGE, MAX_PACKS, TaskKeyError, check_build};
use crate::task_pack::MAX_RECORD_BYTES;

/// The first manifest version: one pack, no large object. Written whenever the manifest names
/// nothing more.
pub const MANIFEST_VERSION: u32 = 1;

/// The second manifest version: adds `packs` and `large`.
pub const MANIFEST_VERSION_2: u32 = 2;

/// The largest manifest record: a larger one is refused before it is read.
pub const MAX_MANIFEST_BYTES: u64 = 4 << 20;

/// The longest output path or symlink target, in bytes.
pub const MAX_PATH_BYTES: usize = 4096;

/// The SHA-256 of no bytes: the digest of an empty file, which no pack holds a record for
/// (every content-addressable store has the empty blob implicitly).
pub const EMPTY_DIGEST: Digest = Digest::new(
    DigestFunction::Sha256,
    [
        0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9,
        0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52,
        0xb8, 0x55,
    ],
    0,
);

/// Why a path or a symlink target is not allowed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// The string is empty.
    #[error("empty")]
    Empty,
    /// The string is longer than [`MAX_PATH_BYTES`].
    #[error("{0} bytes, more than {MAX_PATH_BYTES}")]
    TooLong(usize),
    /// The string holds a NUL byte.
    #[error("holds a NUL byte")]
    Nul,
    /// An output path starts with `/`.
    #[error("starts with `/`")]
    Absolute,
    /// An output path has a `..` component.
    #[error("has a `..` component")]
    ParentComponent,
}

/// Why a manifest is refused.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The manifest is longer than [`MAX_MANIFEST_BYTES`]. It was not read.
    #[error("the manifest has {len} bytes, more than {MAX_MANIFEST_BYTES}")]
    TooLarge {
        /// The length given.
        len: u64,
    },
    /// The bytes are not the wire form: not JSON, a field of the wrong type, an unknown
    /// field, a duplicate key or a missing field.
    #[error("the manifest is not valid: {0}")]
    Json(#[from] serde_json::Error),
    /// The version is neither [`MANIFEST_VERSION`] nor [`MANIFEST_VERSION_2`].
    #[error("manifest version {0}, this build reads {MANIFEST_VERSION} and {MANIFEST_VERSION_2}")]
    Version(u32),
    /// A version 1 manifest holds a field only version 2 has.
    #[error("manifest version {MANIFEST_VERSION} has no field {0:?}")]
    UnexpectedField(&'static str),
    /// A version 2 manifest lacks a field it requires.
    #[error("manifest version {MANIFEST_VERSION_2} requires the field {0:?}")]
    MissingField(&'static str),
    /// A version 2 body that has one pack, no large object and no `inputs`: that manifest is
    /// spelled as version 1.
    #[error(
        "manifest version {MANIFEST_VERSION_2} with one pack, no large object and no inputs is spelled as version {MANIFEST_VERSION}"
    )]
    NotV2,
    /// A manifest that names more than one pack, given to a reader of one pack: only the `_with`
    /// forms read a multi-pack run, because the others would take pack 0 for all of it.
    #[error("the manifest names {0} packs; this reader takes one")]
    MultiPack(u8),
    /// `packs` is 0 or more than [`MAX_PACKS`].
    #[error("packs is {0}, not in 1..={MAX_PACKS}")]
    Packs(u8),
    /// A large slot at or above [`MAX_LARGE`].
    #[error("large slot {0} is not below {MAX_LARGE}")]
    LargeSlot(u8),
    /// Two large entries have one slot.
    #[error("large slot {0} is listed twice")]
    DuplicateLargeSlot(u8),
    /// Two large entries have one digest: a digest is in exactly one place.
    #[error("the digest {0} is listed as a large object twice")]
    DuplicateLargeDigest(Digest),
    /// A large entry of at most [`MAX_RECORD_BYTES`]: a blob that small is a pack record.
    #[error("large slot {slot} has {size} bytes, not more than the {MAX_RECORD_BYTES} of a record")]
    LargeTooSmall {
        /// The slot.
        slot: u8,
        /// The size given.
        size: u64,
    },
    /// A large entry over the large cap.
    #[error("large slot {slot} has {size} bytes, more than the large cap {cap}")]
    LargeTooBig {
        /// The slot.
        slot: u8,
        /// The size given.
        size: u64,
        /// The cap the reader was given.
        cap: u64,
    },
    /// A large entry that no output names.
    #[error("large slot {slot} holds {digest}, which no output names")]
    LargeUnnamed {
        /// The slot.
        slot: u8,
        /// The digest given.
        digest: Digest,
    },
    /// The lease is not a build id.
    #[error("the lease: {0}")]
    Lease(#[from] TaskKeyError),
    /// A digest field is not `<64 lowercase hex>/<size>`.
    #[error("{field}: {source}")]
    Digest {
        /// The field, with the position of a list entry.
        field: String,
        /// Why the text is not a digest.
        source: ParseDigestError,
    },
    /// A digest of size 0 that is not the digest of no bytes.
    #[error("{field}: {digest} has size 0 but is not the digest of no bytes")]
    ZeroSizeNotEmpty {
        /// The field, with the position of a list entry.
        field: String,
        /// The digest given.
        digest: Digest,
    },
    /// `stdout` or `stderr` is present and empty: an empty stream is left out.
    #[error("{0} is the empty digest; an empty stream is omitted")]
    EmptyStream(&'static str),
    /// An output path is not allowed.
    #[error("output path {path:?}: {reason}")]
    Path {
        /// The path given.
        path: String,
        /// Why it is refused.
        reason: PathError,
    },
    /// A symlink target is not allowed.
    #[error("target of symlink {path:?}: {reason}")]
    Target {
        /// The symlink's path.
        path: String,
        /// Why the target is refused.
        reason: PathError,
    },
    /// Two outputs, of any kind, have one path.
    #[error("output path {0:?} is listed twice")]
    DuplicatePath(String),
    /// A timing or usage number that does not fit a signed 64-bit integer.
    #[error("{field} is {value}, more than {}", i64::MAX)]
    Advisory {
        /// The field.
        field: &'static str,
        /// The number given.
        value: u64,
    },
}

/// An output file: a path, the digest of its content and whether it is executable. An empty
/// file has [`EMPTY_DIGEST`] and no record in the pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputFile {
    /// The path, relative to the action's working directory.
    pub path: String,
    /// The digest of the content.
    pub digest: Digest,
    /// Whether the executable bit is set.
    pub is_executable: bool,
}

/// An output directory: its path and the digest of its REAPI `Tree` blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputDirectory {
    /// The path, relative to the action's working directory.
    pub path: String,
    /// The digest of the `Tree`.
    pub tree: Digest,
}

/// An output symlink: its path and what it points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSymlink {
    /// The path, relative to the action's working directory.
    pub path: String,
    /// The link's target as written; it may be absolute or leave the working directory.
    pub target: String,
}

/// A large object: the raw object under key slot `L<slot>`, holding one blob over
/// [`MAX_RECORD_BYTES`]. The size is the digest's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LargeEntry {
    /// The index `i` of the key slot `L<i>`.
    pub slot: u8,
    /// The digest of the blob, and so its size.
    pub digest: Digest,
}

impl LargeEntry {
    /// The size of the blob, in bytes.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.digest.size_bytes
    }
}

/// Where the bytes of a task's inputs came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SourceBytes {
    /// From the read listener, one blob per request.
    pub listener_get: u64,
    /// From the read listener's batch request.
    pub listener_batch: u64,
    /// From a node-local read cache.
    pub cache_hit: u64,
    /// Directly from the tier store.
    pub tierstore_direct: u64,
}

/// What a task measured while fetching its inputs. A field the writer leaves out is 0.
///
/// ADVISORY, NEVER TRUSTED: the task writes it and ingest only carries it into metrics. It
/// decides nothing: not what is stored, not the action result, not whether the run is accepted
/// (beyond each number fitting an `i64`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct InputStats {
    /// Blobs fetched.
    pub blobs: u64,
    /// Files in the input tree.
    pub files: u64,
    /// Directories in the input tree.
    pub directories: u64,
    /// Requests made.
    pub requests: u64,
    /// Requests that were retried.
    pub retries: u64,
    /// The most requests in flight at once.
    pub max_in_flight: u64,
    /// Bytes written to the scratch directory.
    pub bytes_written_scratch: u64,
    /// Time walking the input tree, in milliseconds.
    pub walk_ms: u64,
    /// Time fetching, in milliseconds.
    pub fetch_ms: u64,
    /// Time hashing, in milliseconds.
    pub hash_ms: u64,
    /// Time writing to scratch, in milliseconds.
    pub write_ms: u64,
    /// The median request time, in milliseconds.
    pub request_p50_ms: u64,
    /// The 99th percentile request time, in milliseconds.
    pub request_p99_ms: u64,
    /// Bytes by where they came from.
    pub bytes_by_source: SourceBytes,
}

impl InputStats {
    /// Every number with its field name, for the `i64` check.
    fn numbers(&self) -> [(&'static str, u64); 17] {
        let source = &self.bytes_by_source;
        [
            ("inputs.blobs", self.blobs),
            ("inputs.files", self.files),
            ("inputs.directories", self.directories),
            ("inputs.requests", self.requests),
            ("inputs.retries", self.retries),
            ("inputs.max_in_flight", self.max_in_flight),
            ("inputs.bytes_written_scratch", self.bytes_written_scratch),
            ("inputs.walk_ms", self.walk_ms),
            ("inputs.fetch_ms", self.fetch_ms),
            ("inputs.hash_ms", self.hash_ms),
            ("inputs.write_ms", self.write_ms),
            ("inputs.request_p50_ms", self.request_p50_ms),
            ("inputs.request_p99_ms", self.request_p99_ms),
            ("inputs.bytes_by_source.listener_get", source.listener_get),
            (
                "inputs.bytes_by_source.listener_batch",
                source.listener_batch,
            ),
            ("inputs.bytes_by_source.cache_hit", source.cache_hit),
            (
                "inputs.bytes_by_source.tierstore_direct",
                source.tierstore_direct,
            ),
        ]
    }
}

/// How long the task's steps took, in milliseconds. Advisory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timing {
    /// Fetching the inputs.
    pub fetch_ms: u64,
    /// Running the command.
    pub run_ms: u64,
    /// Collecting the outputs and writing the pack.
    pub pack_ms: u64,
}

/// What the command used. Advisory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// CPU time, in microseconds.
    pub cpu_usage_us: u64,
    /// The peak resident memory, in bytes.
    pub memory_peak_bytes: u64,
}

/// What a task says its action did.
///
/// Fields are public so the task can build one; [`TaskManifest::to_json`] refuses to write a
/// manifest that [`TaskManifest::validate`] refuses, so what is written always reads back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskManifest {
    /// The build id the frontend issued: the middle part of the pack key.
    pub lease: String,
    /// The digest of the action that ran.
    pub action: Digest,
    /// The command's exit code.
    pub exit_code: i32,
    /// The standard output, or `None` when it was empty.
    pub stdout: Option<Digest>,
    /// The standard error, or `None` when it was empty.
    pub stderr: Option<Digest>,
    /// The output files.
    pub output_files: Vec<OutputFile>,
    /// The output directories.
    pub output_directories: Vec<OutputDirectory>,
    /// The output symlinks.
    pub output_symlinks: Vec<OutputSymlink>,
    /// Step durations.
    pub timing: Timing,
    /// Resource use.
    pub usage: Usage,
    /// How many of packs `0..` were written: 1 to [`MAX_PACKS`]. Pack 0 holds the manifest.
    pub packs: u8,
    /// The large objects, by slot. Empty for a version 1 manifest.
    pub large: Vec<LargeEntry>,
    /// What the task measured fetching its inputs, if it says. Advisory: see [`InputStats`].
    pub inputs: Option<InputStats>,
}

impl TaskManifest {
    /// Reads the manifest record `bytes` for a consumer of one pack: a manifest of more than one
    /// pack (or with a large object) is refused; [`TaskManifest::parse_with`] takes those.
    ///
    /// # Errors
    /// [`ManifestError::TooLarge`] first, before anything is parsed; then the first of
    /// [`ManifestError::Json`], [`ManifestError::Version`], [`ManifestError::Digest`] and the
    /// refusals of [`TaskManifest::validate`], and [`ManifestError::MultiPack`].
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        let manifest = Self::parse_with(bytes, 0)?;
        manifest.check_single_pack()?;
        Ok(manifest)
    }

    /// [`TaskManifest::parse`] for a reader that accepts large objects of up to `large_max`
    /// bytes. A `large_max` of 0 accepts none, as [`TaskManifest::parse`] does.
    ///
    /// # Errors
    /// Those of [`TaskManifest::parse`], and [`ManifestError::UnexpectedField`] or
    /// [`ManifestError::MissingField`] for the fields of the version.
    pub fn parse_with(bytes: &[u8], large_max: u64) -> Result<Self, ManifestError> {
        check_len(bytes.len() as u64)?;
        let wire: Wire = serde_json::from_slice(bytes)?;
        let wire_version = wire.version;
        match wire.version {
            MANIFEST_VERSION => {
                if wire.packs.is_some() {
                    return Err(ManifestError::UnexpectedField("packs"));
                }
                if wire.large.is_some() {
                    return Err(ManifestError::UnexpectedField("large"));
                }
                if wire.inputs.is_some() {
                    return Err(ManifestError::UnexpectedField("inputs"));
                }
            }
            MANIFEST_VERSION_2 => {
                if wire.packs.is_none() {
                    return Err(ManifestError::MissingField("packs"));
                }
                if wire.large.is_none() {
                    return Err(ManifestError::MissingField("large"));
                }
            }
            other => return Err(ManifestError::Version(other)),
        }
        let manifest = wire.into_manifest()?;
        manifest.validate_with(large_max)?;
        if wire_version == MANIFEST_VERSION_2 && manifest.version() == MANIFEST_VERSION {
            return Err(ManifestError::NotV2);
        }
        Ok(manifest)
    }

    /// The canonical JSON of the manifest: compact, in the field order of the module docs.
    ///
    /// # Errors
    /// The refusals of [`TaskManifest::validate`], and [`ManifestError::TooLarge`] when the
    /// JSON would pass [`MAX_MANIFEST_BYTES`].
    pub fn to_json(&self) -> Result<Vec<u8>, ManifestError> {
        self.validate()?;
        self.to_json_with(0)
    }

    /// [`TaskManifest::to_json`] for a writer whose large objects may have up to `large_max`
    /// bytes. Version 1 is written for one pack and no large object, version 2 otherwise.
    ///
    /// # Errors
    /// Those of [`TaskManifest::to_json`], checked against `large_max`.
    pub fn to_json_with(&self, large_max: u64) -> Result<Vec<u8>, ManifestError> {
        self.validate_with(large_max)?;
        let json = serde_json::to_vec(&Wire::from_manifest(self))?;
        check_len(json.len() as u64)?;
        Ok(json)
    }

    /// Checks every rule that is not the JSON's own: the lease is a build id; a digest of size 0
    /// is [`EMPTY_DIGEST`]; `stdout` and `stderr` are not empty; paths are relative, without
    /// `..`, NUL or empty, at most [`MAX_PATH_BYTES`], and unique across files, directories and
    /// symlinks; targets are not empty, NUL-free and at most [`MAX_PATH_BYTES`]; timing and
    /// usage fit an `i64`.
    ///
    /// # Errors
    /// The first rule that fails, in the order of the fields.
    pub fn validate(&self) -> Result<(), ManifestError> {
        self.validate_with(0)?;
        self.check_single_pack()
    }

    /// The rule of the plain forms, which are for a consumer of one pack: more than one pack is
    /// [`ManifestError::MultiPack`], so pack 0 is never taken for all of a multi-pack run.
    pub(crate) fn check_single_pack(&self) -> Result<(), ManifestError> {
        if self.packs > 1 {
            return Err(ManifestError::MultiPack(self.packs));
        }
        Ok(())
    }

    /// [`TaskManifest::validate`] for a reader that accepts large objects of up to `large_max`
    /// bytes, and also: `packs` is 1 to [`MAX_PACKS`]; large slots are below [`MAX_LARGE`] and
    /// unique; large digests are unique; each large size is over [`MAX_RECORD_BYTES`] and at most
    /// `large_max`; each large digest is named by an output. Which pack holds each other digest
    /// is not checked here.
    ///
    /// # Errors
    /// The first rule that fails, the large rules after those of [`TaskManifest::validate`].
    pub fn validate_with(&self, large_max: u64) -> Result<(), ManifestError> {
        check_build(&self.lease)?;
        check_digest("action", &self.action)?;
        for (field, stream) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
            if let Some(digest) = stream {
                check_digest(field, digest)?;
                if digest.size_bytes == 0 {
                    return Err(ManifestError::EmptyStream(field));
                }
            }
        }
        let mut seen = BTreeSet::new();
        for (i, file) in self.output_files.iter().enumerate() {
            check_path(&mut seen, &file.path)?;
            check_digest(&format!("output_files[{i}].digest"), &file.digest)?;
        }
        for (i, dir) in self.output_directories.iter().enumerate() {
            check_path(&mut seen, &dir.path)?;
            check_digest(&format!("output_directories[{i}].tree"), &dir.tree)?;
        }
        for link in &self.output_symlinks {
            check_path(&mut seen, &link.path)?;
            check_text(&link.target).map_err(|reason| ManifestError::Target {
                path: link.path.clone(),
                reason,
            })?;
        }
        for (field, value) in [
            ("timing.fetch_ms", self.timing.fetch_ms),
            ("timing.run_ms", self.timing.run_ms),
            ("timing.pack_ms", self.timing.pack_ms),
            ("usage.cpu_usage_us", self.usage.cpu_usage_us),
            ("usage.memory_peak_bytes", self.usage.memory_peak_bytes),
        ]
        .into_iter()
        .chain(self.inputs.iter().flat_map(|i| i.numbers()))
        {
            if i64::try_from(value).is_err() {
                return Err(ManifestError::Advisory { field, value });
            }
        }
        self.check_packs_and_large(large_max)
    }

    fn check_packs_and_large(&self, large_max: u64) -> Result<(), ManifestError> {
        if !(1..=MAX_PACKS).contains(&self.packs) {
            return Err(ManifestError::Packs(self.packs));
        }
        let mut slots = BTreeSet::new();
        let mut digests = BTreeSet::new();
        for entry in &self.large {
            let (slot, size) = (entry.slot, entry.size());
            if slot >= MAX_LARGE {
                return Err(ManifestError::LargeSlot(slot));
            }
            if !slots.insert(slot) {
                return Err(ManifestError::DuplicateLargeSlot(slot));
            }
            if size <= MAX_RECORD_BYTES {
                return Err(ManifestError::LargeTooSmall { slot, size });
            }
            if size > large_max {
                let cap = large_max;
                return Err(ManifestError::LargeTooBig { slot, size, cap });
            }
            if !digests.insert(entry.digest) {
                return Err(ManifestError::DuplicateLargeDigest(entry.digest));
            }
            if !self.output_digests().any(|d| d == entry.digest) {
                let digest = entry.digest;
                return Err(ManifestError::LargeUnnamed { slot, digest });
            }
        }
        Ok(())
    }

    /// The manifest version this manifest is written as.
    #[must_use]
    pub fn version(&self) -> u32 {
        if self.packs == 1 && self.large.is_empty() && self.inputs.is_none() {
            MANIFEST_VERSION
        } else {
            MANIFEST_VERSION_2
        }
    }

    /// The digests of the outputs that need a record in the pack or a copy already in the store:
    /// `stdout`, `stderr`, each file and each tree, without those of size 0 (implicit in every
    /// store). Repeats are kept; the action is an input, not an output, and is not listed.
    pub fn output_digests(&self) -> impl Iterator<Item = Digest> + '_ {
        self.stdout
            .iter()
            .chain(&self.stderr)
            .copied()
            .chain(self.output_files.iter().map(|f| f.digest))
            .chain(self.output_directories.iter().map(|d| d.tree))
            .filter(|d| d.size_bytes != 0)
    }
}

fn check_len(len: u64) -> Result<(), ManifestError> {
    if len > MAX_MANIFEST_BYTES {
        return Err(ManifestError::TooLarge { len });
    }
    Ok(())
}

fn check_digest(field: &str, digest: &Digest) -> Result<(), ManifestError> {
    if digest.size_bytes == 0 && *digest != EMPTY_DIGEST {
        return Err(ManifestError::ZeroSizeNotEmpty {
            field: field.to_owned(),
            digest: *digest,
        });
    }
    Ok(())
}

/// The rules a path and a symlink target share: not empty, not too long, no NUL.
fn check_text(text: &str) -> Result<(), PathError> {
    if text.is_empty() {
        return Err(PathError::Empty);
    }
    if text.len() > MAX_PATH_BYTES {
        return Err(PathError::TooLong(text.len()));
    }
    if text.contains('\0') {
        return Err(PathError::Nul);
    }
    Ok(())
}

fn check_path(seen: &mut BTreeSet<String>, path: &str) -> Result<(), ManifestError> {
    let refuse = |reason| ManifestError::Path {
        path: path.to_owned(),
        reason,
    };
    check_text(path).map_err(refuse)?;
    if path.starts_with('/') {
        return Err(refuse(PathError::Absolute));
    }
    if path.split('/').any(|part| part == "..") {
        return Err(refuse(PathError::ParentComponent));
    }
    if !seen.insert(path.to_owned()) {
        return Err(ManifestError::DuplicatePath(path.to_owned()));
    }
    Ok(())
}

fn digest_of(field: String, text: &str) -> Result<Digest, ManifestError> {
    Digest::parse(DigestFunction::Sha256, text)
        .map_err(|source| ManifestError::Digest { field, source })
}

// The wire form. Private: the typed manifest above is the API, so a caller cannot build a
// manifest the reader would disagree with about spelling.

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u32,
    lease: String,
    action: String,
    exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stderr: Option<String>,
    output_files: Vec<WireFile>,
    output_directories: Vec<WireDirectory>,
    output_symlinks: Vec<WireSymlink>,
    timing: WireTiming,
    usage: WireUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    packs: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    large: Option<Vec<WireLarge>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inputs: Option<InputStats>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireLarge {
    slot: u8,
    digest: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFile {
    path: String,
    digest: String,
    is_executable: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireDirectory {
    path: String,
    tree: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSymlink {
    path: String,
    target: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTiming {
    fetch_ms: u64,
    run_ms: u64,
    pack_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireUsage {
    cpu_usage_us: u64,
    memory_peak_bytes: u64,
}

impl Wire {
    fn from_manifest(m: &TaskManifest) -> Self {
        Self {
            version: m.version(),
            lease: m.lease.clone(),
            action: m.action.to_string(),
            exit_code: m.exit_code,
            stdout: m.stdout.map(|d| d.to_string()),
            stderr: m.stderr.map(|d| d.to_string()),
            output_files: m
                .output_files
                .iter()
                .map(|f| WireFile {
                    path: f.path.clone(),
                    digest: f.digest.to_string(),
                    is_executable: f.is_executable,
                })
                .collect(),
            output_directories: m
                .output_directories
                .iter()
                .map(|d| WireDirectory {
                    path: d.path.clone(),
                    tree: d.tree.to_string(),
                })
                .collect(),
            output_symlinks: m
                .output_symlinks
                .iter()
                .map(|l| WireSymlink {
                    path: l.path.clone(),
                    target: l.target.clone(),
                })
                .collect(),
            timing: WireTiming {
                fetch_ms: m.timing.fetch_ms,
                run_ms: m.timing.run_ms,
                pack_ms: m.timing.pack_ms,
            },
            usage: WireUsage {
                cpu_usage_us: m.usage.cpu_usage_us,
                memory_peak_bytes: m.usage.memory_peak_bytes,
            },
            packs: (m.version() == MANIFEST_VERSION_2).then_some(m.packs),
            large: (m.version() == MANIFEST_VERSION_2).then(|| {
                m.large
                    .iter()
                    .map(|l| WireLarge {
                        slot: l.slot,
                        digest: l.digest.to_string(),
                    })
                    .collect()
            }),
            inputs: m.inputs,
        }
    }

    fn into_manifest(self) -> Result<TaskManifest, ManifestError> {
        let stream = |field: &str, text: Option<String>| {
            text.map(|t| digest_of(field.to_owned(), &t)).transpose()
        };
        let mut output_files = Vec::with_capacity(self.output_files.len());
        for (i, f) in self.output_files.into_iter().enumerate() {
            output_files.push(OutputFile {
                digest: digest_of(format!("output_files[{i}].digest"), &f.digest)?,
                path: f.path,
                is_executable: f.is_executable,
            });
        }
        let mut output_directories = Vec::with_capacity(self.output_directories.len());
        for (i, d) in self.output_directories.into_iter().enumerate() {
            output_directories.push(OutputDirectory {
                tree: digest_of(format!("output_directories[{i}].tree"), &d.tree)?,
                path: d.path,
            });
        }
        let mut large = Vec::new();
        for (i, l) in self.large.into_iter().flatten().enumerate() {
            large.push(LargeEntry {
                digest: digest_of(format!("large[{i}].digest"), &l.digest)?,
                slot: l.slot,
            });
        }
        Ok(TaskManifest {
            action: digest_of("action".to_owned(), &self.action)?,
            lease: self.lease,
            exit_code: self.exit_code,
            stdout: stream("stdout", self.stdout)?,
            stderr: stream("stderr", self.stderr)?,
            output_files,
            output_directories,
            output_symlinks: self
                .output_symlinks
                .into_iter()
                .map(|l| OutputSymlink {
                    path: l.path,
                    target: l.target,
                })
                .collect(),
            timing: Timing {
                fetch_ms: self.timing.fetch_ms,
                run_ms: self.timing.run_ms,
                pack_ms: self.timing.pack_ms,
            },
            usage: Usage {
                cpu_usage_us: self.usage.cpu_usage_us,
                memory_peak_bytes: self.usage.memory_peak_bytes,
            },
            packs: self.packs.unwrap_or(1),
            large,
            inputs: self.inputs,
        })
    }
}
