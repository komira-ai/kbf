//! The packing client: many blobs in one object, and the blob index that finds them.
//!
//! A pack is one object of an object store that holds many blobs, in the segment layout of
//! `kbf-segments` (the blobs back to back, then a footer that indexes them). The pack index
//! (`kbf_state::PackIndex`) maps a blob's digest to the pack, the offset and the length, so a
//! reader asks the store for one range of one object. This crate is the client side of both:
//!
//! - [`Packer`] owns the **write rule**: it writes a pack to the store with a create-only
//!   write, and only after the store has acknowledged it does it record the pack in the
//!   index. A row never points at a pack that is not stored. A blob the index has already is
//!   not packed again, a pack is cut at a byte limit and a blob limit, and a blob too big for
//!   a pack is a pack of its own. [`check_create_only`] tests a store's create-only claim at
//!   startup.
//! - [`Reader`] finds blobs through the index, reads the packs with ranged requests (blobs
//!   close together in a pack are one request), hashes every blob before it hands it out, and
//!   tells the index which blobs it served, in batches.
//! - [`ReadCache`] is the bounded on-disk cache of blobs a reader keeps in front of the
//!   store: least recently used out first, written with a temporary file and a rename,
//!   checked by digest on every read.
//!
//! Pack keys are `cas/d/<node>/<boot>/<seq>` for a daemon and `cas/f/<frontend>/<boot>/<seq>`
//! for a frontend ([`PackKeys`]); the sequence number is sixteen hex digits, so a listing of
//! a writer's prefix is in write order.
//!
//! The index's calls block, so the packer and the reader run them on the async runtime's
//! blocking pool, and the cache's file calls too.

mod blocking;
mod cache;
mod digest;
mod error;
mod keys;
mod packer;
mod reader;

pub use cache::ReadCache;
pub use digest::blob_digest;
pub use error::{CacheError, PackError, ReadError};
pub use keys::{PackKeys, WriterKind};
pub use packer::{
    Blob, FRONTEND_PACK_BYTES, Packed, Packer, PackerConfig, StoredPack, check_create_only,
};
pub use reader::{DEFAULT_CONCURRENCY, DEFAULT_GAP, Fetch, Reader, ReaderConfig};
