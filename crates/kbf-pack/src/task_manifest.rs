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

use kbf_types::{Digest, DigestFunction, ParseDigestError};

use crate::task_key::TaskKeyError;

/// The manifest version this build reads and writes.
pub const MANIFEST_VERSION: u32 = 1;

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
    /// The version is not [`MANIFEST_VERSION`].
    #[error("manifest version {0}, this build reads {MANIFEST_VERSION}")]
    Version(u32),
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
}

impl TaskManifest {
    /// Reads the manifest record `bytes`.
    ///
    /// # Errors
    /// [`ManifestError::TooLarge`] first, before anything is parsed; then the first of
    /// [`ManifestError::Json`], [`ManifestError::Version`], [`ManifestError::Digest`] and the
    /// refusals of [`TaskManifest::validate`].
    #[expect(unused_variables, reason = "red-first stub")]
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        unimplemented!("S5 PR-A: red stub")
    }

    /// The canonical JSON of the manifest: compact, in the field order of the module docs.
    ///
    /// # Errors
    /// The refusals of [`TaskManifest::validate`], and [`ManifestError::TooLarge`] when the
    /// JSON would pass [`MAX_MANIFEST_BYTES`].
    pub fn to_json(&self) -> Result<Vec<u8>, ManifestError> {
        unimplemented!("S5 PR-A: red stub")
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
        unimplemented!("S5 PR-A: red stub")
    }

    /// The digests of the outputs that need a record in the pack or a copy already in the store:
    /// `stdout`, `stderr`, each file and each tree, without those of size 0 (implicit in every
    /// store). Repeats are kept; the action is an input, not an output, and is not listed.
    pub fn output_digests(&self) -> impl Iterator<Item = Digest> + '_ {
        std::iter::empty()
    }
}
