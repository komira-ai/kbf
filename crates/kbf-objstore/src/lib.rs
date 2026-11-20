//! The `ObjectStore` trait with an in-memory fake, a local-directory store and an S3
//! backend, the conformance suite every backend must pass, every object under a prefix
//! as a stream that holds one page at a time ([`listing`]), the reader that gets every
//! segment's index back from the segments' footers ([`footers`]), and the batch lookup of
//! a SeaweedFS filer that says which of many objects it holds ([`filer`]).
//!
//! kbf names every key and records every location itself, so it needs no consistent
//! listing and no read-after-write: when it asks whether objects exist, it asks for them
//! by key ([`ObjectStore::len`], [`filer`]). What it needs of a store is in
//! [`ObjectStore`]; what a store may additionally claim (conditional writes, Object Lock)
//! is in [`Capabilities`], and [`conformance`] checks the base contract and every claim.
//!
//! One store value addresses one bucket.

pub mod conformance;
pub mod filer;
pub mod footers;
mod fs_store;
mod kbf_keys;
mod memory;
pub mod probe;
mod released;

pub use fs_store::{
    Floor, FsStore, FsStoreError, Room, Scanned, ShardScan, SpaceSource, objects_source,
    statvfs_source,
};
pub use kbf_keys::{KbfKeyError, kbf_key, kbf_prefix};
pub use kbf_threads::disk::Space;
pub use memory::MemoryStore;
pub use objstore_s3::{
    BodyStream, ByteRange, Capabilities, KeyError, KeyPrefix, ListPage, ListToken, ObjectInfo,
    ObjectKey, ObjectStore, ObjectStoreError, PageSize, gather,
};
pub use objstore_s3::{listing, s3};
pub use released::{Released, StillHeld};
