//! The blobs a lease reads and writes: the [`Cas`] trait and its helpers, from
//! `kbf-transfer`, whose [`kbf_transfer::Transfers`] is the store the daemon binary's
//! runtimes read and write through (one per process, shared by every lease).
//!
//! A runtime fetches an action, its command and its input tree through a `Cas`, and
//! stores outputs, stdout and stderr through it; a driver hashes an action's outputs
//! once and stores them with [`Cas::put_records`] (the native driver gathers them in a
//! [`Spool`] first). Blob bytes never travel on the
//! `kbf.worker.v1` stream; they go over the engine's own connection.

pub use kbf_transfer::cas::*;
pub use kbf_transfer::{
    FileSource, OPEN_DIRECTORY, OPEN_FILE, Record, RecordHasher, Source, Spool, Spooled, Stamp,
    descend, proto_digest,
};
