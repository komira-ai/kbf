//! The blobs a lease reads and writes: the [`Cas`] trait and its helpers, from
//! `kbf-transfer`, whose [`kbf_transfer::Transfers`] is the store the daemon binary's
//! runtimes read and write through (one per process, shared by every lease).
//!
//! A runtime fetches an action, its command and its input tree through a `Cas`, and
//! stores outputs, stdout and stderr through it. Blob bytes never travel on the
//! `kbf.worker.v1` stream; they go over the engine's own connection.

pub use kbf_transfer::cas::*;
