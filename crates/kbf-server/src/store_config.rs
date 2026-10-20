//! The cold-store registration as storage nodes see it: where the frontend reads it from,
//! how a node fetches it, and how the frontend learns which nodes applied it.
//!
//! **Delivery** (`docs/design/object-store-registration.md`): the registration is durable
//! desired state with a generation. Every `Membership` answer carries the generation
//! (`objstore_generation`); a node whose applied generation differs calls
//! `ColdStore.GetStoreConfig` ([`service`]) over its storage certificate and reports what
//! it did in its next `Heartbeat` (`applied_objstore_generation`, `apply_status`), which
//! [`acks`] keeps in the state store with a time to live. Nothing here reaches a daemon: the daemon's wire has no
//! such field (`kbf-proto`'s `tests/daemon_wire.rs`).
//!
//! **The source** ([`StoreConfigSource`]) is whatever holds the registration: the state
//! store's rows and the secret file. It is a trait because the rows and the files belong
//! to the registration storage and the operator routes, which this module does not
//! reach into; a test source stands in for them. The contract that matters here is one
//! method: [`StoreConfigSource::snapshot`] returns the connection, the binding and the
//! secret of ONE generation, so a node never pairs one generation's endpoint with another's
//! key. This module never reads the pieces apart.
//!
//! **No writer.** Frontend children run side by side on one state store and every one serves
//! the registration; nothing here is gated on a single writer, and the acks are shared
//! through the store ([`acks`]).
//!
//! **Generations go both ways.** A node refetches on any generation that differs from its
//! own, not only a higher one: a state store restored from a backup may hold a lower
//! generation than nodes already applied, and a node that ignored it would never
//! converge.

pub mod acks;
pub mod service;

use kbf_proto::kbf::enroll::v1::StoreConfig;

/// One consistent reading of the registration.
#[derive(Debug)]
pub struct Snapshot {
    /// The registration generation the reading is of; 0 when nothing is registered.
    pub generation: u64,
    /// The store and its keys; `None` exactly when `generation` is 0.
    pub config: Option<StoreConfig>,
}

/// Why the registration cannot be read now. The messages are fixed text: none carries a
/// path, a row or an error of the store, so a status built from one reveals no secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// A registered secret is missing or readable by others: fail closed.
    #[error("the registered store's secret is not available")]
    SecretUnavailable,
    /// The state store failed.
    #[error("the state store failed")]
    Store,
}

/// Where the frontend reads the registration from.
///
/// Both methods block (they read the state store and a file): callers run them on a
/// blocking thread.
pub trait StoreConfigSource: Send + Sync {
    /// The registration generation, cheaply: it is read at every heartbeat and must not
    /// read the secret.
    ///
    /// # Errors
    /// [`SourceError`]; a heartbeat answers 0 ("cannot say") rather than failing.
    fn generation(&self) -> Result<u64, SourceError>;

    /// The registration, whole and consistent (see the module docs).
    ///
    /// # Errors
    /// [`SourceError`].
    fn snapshot(&self) -> Result<Snapshot, SourceError>;
}

#[cfg(test)]
mod tests;
