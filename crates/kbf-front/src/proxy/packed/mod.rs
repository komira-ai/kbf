//! The frontend's blobs in packs: what a client uploads is written to an object store as
//! packs (many blobs in one object, in the segment layout of `kbf-segments`) and recorded in
//! the pack index before the upload is acknowledged, and what a client reads is found through
//! the index and read from its pack with a ranged request. Off unless the frontend is given
//! [`Packs`] ([`Proxy::with_packs`](super::Proxy::with_packs),
//! [`Backend::remote_over`](crate::Backend::remote_over) and
//! [`Backend::routed_over`](crate::Backend::routed_over)); without them nothing here runs and
//! the calls take the paths they always took.
//!
//! **Writes.** A `BatchUpdateBlobs` is checked blob by blob (digest, no compression), the blobs
//! that pass are packed together, and each answers OK only once the pack is stored and the
//! index has its row (the write rule of `kbf-pack`). A ByteStream `Write` of a blob of at most
//! [`Packs::max_blob_bytes`] is received whole, hashed and packed the same way; a larger one,
//! and any upload of a compressed blob, goes to the storage nodes as it does without packs. A
//! failed write answers UNAVAILABLE; the packs finished before the failure stay recorded (the
//! packer records each pack before it starts the next), and a write sent again packs only the
//! rest.
//!
//! **Reads.** The index is asked first. A blob it has is read with ranged requests of its
//! pack (the blob's bytes and no others), hashed, and served; a read of a range of it
//! (`read_offset`, `read_limit`) is cut from the hashed blob. A blob the index lacks, or whose
//! pack is gone, is read from the storage nodes as without packs. A blob whose pack bytes are
//! not its digest is never served: the call fails UNAVAILABLE and is counted. A
//! `BatchReadBlobs` asks the packs for all of its blobs together, so the index is asked once
//! and blobs close together in a pack are one request; when the index or the store fails, every
//! blob of the batch the packs were asked for fails with it.
//!
//! **FindMissingBlobs** asks the index and then, for the digests it lacks, the storage nodes,
//! so a blob held by either counts as present.
//!
//! **Not packed:** `QueryWriteStatus` (a blob is reported as the storage node reports it), a
//! ByteStream `Read` of a compressed blob, and blobs over [`Packs::max_blob_bytes`].

mod bytestream;
mod cas;
mod metrics;
mod store;

pub(in crate::proxy) use self::bytestream::{applies, read as read_stream, write as write_stream};
pub(in crate::proxy) use self::cas::{Served, find_missing, read_blob, read_many, store_batch};
pub use self::metrics::PackMetrics;
pub use self::store::{MAX_PACKED_BLOB_BYTES, PackSettings, PackStore, Packs};
