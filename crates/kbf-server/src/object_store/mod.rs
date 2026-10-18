//! The registration of the farm's object store as durable desired state: connections
//! (a named endpoint with a key pair), bindings (a role to a connection, a bucket and a
//! prefix) and an audit log, kept in the frontend's state store, with each connection's
//! secret key in a versioned file beside it.
//!
//! This module is storage only. It has no routes, no wire and no node side; the control API
//! (`/v1/object-stores`) and the storage nodes' fetch are built on [`Registry`].
//!
//! # What lives where
//!
//! - **State store rows** (`objstore/...`, [`rows`]): the non-secret fields of each
//!   connection, each binding, the registry generation and the audit rows. Never the
//!   secret, and no digest of it (a digest of a short secret is an oracle for it), so
//!   `state-backup` carries no credentials.
//! - **Secret files** ([`SecretFiles`]): `<dir>/{name}.{version:016x}.json`, mode 0600, one
//!   per live connection. `<dir>` must stay out of any filesystem backup of the host.
//!
//! # The write order
//!
//! A change of a secret writes, in this order: a temp file in the same directory, an fsync
//! of it, a rename to its versioned name, an fsync of the directory, and then one state
//! store transaction that compare-and-sets the registry generation, writes the connection
//! row that names the new version and appends the audit row ([`Step`]). The previous
//! version's file is removed only after that transaction commits. A crash at any point
//! leaves the row naming a version whose file exists; what it leaves behind is an orphan
//! file (or temp file) that no row names, and [`Registry::open`] sweeps those once they are older than [`SWEEP_GRACE`].
//!
//! # Two frontends at once
//!
//! Old and new frontend children share one Postgres and this directory. Nothing here
//! assumes a single writer: the generation row is compare-and-set in the store, a
//! file is complete (renamed whole) before any row names it, versions are random so
//! two children never write one name, a reader that finds its version gone re-reads
//! the row, and a sweep leaves young files alone. The in-process mutex only orders
//! one child's own calls.
//!
//! The versions are random, not counters, so two writers (a frontend and the one it
//! handed over to) can never write the same name. In-process failpoint tests check this
//! order; they cannot check that `fsync` reaches the disk.
//!
//! # Generations
//!
//! `objstore/gen` holds the generation and the audit sequence. Every mutation compares
//! and sets that one row in its transaction, so concurrent writers serialize and a lost
//! race is [`RegistryError::Conflict`]. A change bumps the generation by one; a write of
//! the same content bumps nothing and appends an `unchanged` audit row.
//!
//! Callers validate what the types cannot: endpoint syntax, `https` (refused until the S3
//! client speaks TLS), bucket and prefix, and that a probe of the store passed.

mod registry;
mod rows;
mod secrets;

#[cfg(test)]
mod tests;

pub use registry::{Registry, RegistryError, SWEEP_GRACE, Written};
pub use secrets::{SecretError, SecretFiles};

/// A point of the write order at which a test can stop the process (see
/// [`SecretFiles::set_failpoint`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Before the temp file is created and written.
    WriteTemp,
    /// Before the temp file is fsynced.
    SyncTemp,
    /// Before the temp file is renamed to its versioned name.
    Rename,
    /// Before the directory is fsynced.
    SyncDir,
    /// Before the state store transaction that names the new version.
    CommitRow,
    /// Before a secret file is opened for reading.
    ReadFile,
}
