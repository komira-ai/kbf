//! The task pack types: the shared, pure contract of a pack written by a task that has no store
//! credential.
//!
//! A task pack is one object whose record at offset 0 is a JSON manifest ([`TaskManifest`]), under
//! a key the frontend minted ([`TaskPackKey`]), followed by the blobs the manifest names. The
//! task that writes one ([`write_pack`]) and the frontend that ingests it ([`read_manifest`],
//! [`check_shape`]) use these, so the two cannot drift. They do no I/O and hold no secret.
//!
//! This crate is a leaf on purpose: it depends on the segment layout and the digest type only,
//! so a static task binary can link it without the pack index, the object store client or their
//! dependencies. `kbf-pack` re-exports every name below.

mod task_key;
mod task_manifest;
mod task_pack;

pub use task_key::{
    MAC_LEN, MAX_BUILD_BYTES, MAX_LARGE, MAX_PACKS, Slot, TaskKeyError, TaskPackKey, check_build,
    mac_input, mac_input_v1, minute_of,
};
pub use task_manifest::{
    EMPTY_DIGEST, InputStats, LargeEntry, MANIFEST_VERSION, MANIFEST_VERSION_2, MAX_MANIFEST_BYTES,
    MAX_PATH_BYTES, ManifestError, OutputDirectory, OutputFile, OutputSymlink, PathError,
    SourceBytes, TaskManifest, Timing, Usage,
};
pub use task_pack::{
    MAX_PACK_BYTES, MAX_PACK_RECORDS, MAX_RECORD_BYTES, ShapeError, TaskPackError, WritePackError,
    check_records, check_shape, read_manifest, read_manifest_with, write_pack, write_pack_with,
};
