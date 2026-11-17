//! The records of a pull-based update, as the root `kbf-installer` reads them.
//!
//! An update is an [`Offer`] (which release is wanted) and an [`Order`] (when this node may
//! change). This crate holds what the installer and the writer of its records share:
//!
//! - An offer is checked ([`Offer::check`]): **no release bytes pass through the node**, the
//!   root `kbf-installer` pulls the release itself.
//! - An order becomes a small [`OrderRecord`] ([`record`]) in the [order spool](spool): its
//!   components and expected facts are carried by the record, and the installer reads it with
//!   [`OrderRecord::parse`].
//!
//! The writer that fed these from a worker daemon's session with the frontend is gone with the
//! daemon; the formats stay, because a release on the other side of an update reads and writes
//! them.
//!
//! # Trust
//!
//! The client does not decide trust and holds no release. It writes a record into a
//! directory the unprivileged service user owns, and the root installer fetches the release
//! for the commit, verifies the GitHub build attestation offline and the binary's own report,
//! and only then installs. Whatever runs as the service user can write the spool: it can
//! order an early activation of a release that verifies, or write a record the installer
//! refuses. That is a loss of availability, never an unattested install.

#[cfg(test)]
mod fixtures;
mod offer;
pub mod record;
pub mod spool;
mod types;

pub use offer::Refusal;
pub use record::{
    Facts, Locator, MAX_AGE_SECS, MAX_RECORD_BYTES, MAX_RECORDS, OrderRecord, RecordError,
};
pub use types::{
    COMPONENT_NAMES, Component, Expected, MAX_ATTESTATION_BYTES, MAX_COMPONENT_BYTES, Offer, Order,
    Source, Verb,
};
