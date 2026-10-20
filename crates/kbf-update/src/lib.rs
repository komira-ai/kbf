//! The node's end of a pull-based update.
//!
//! The frontend tells a node, over the session the daemon already holds, which release it
//! wants ([`Offer`]) and when this node may change ([`Order`]). This crate is what the
//! node does with that, and nothing about the wire:
//!
//! - [`start`] makes a [`Handle`] the session feeds and a [`Worker`] that runs beside it.
//! - An offer is checked ([`Offer::check`]) and remembered by commit. **No release bytes
//!   pass through the node**: the root `kbf-installer` pulls the release itself.
//! - An order is written into the [order spool](spool) as a small [`OrderRecord`] when it
//!   is for the current session, newer than every order before it, and, for an activation,
//!   when the node holds the offer for its commit (the record carries its components and
//!   expected facts) and no lease is live. An accepted activation closes the update gate
//!   ([`Handle::gate`]); the daemon takes no lease while it is closed.
//! - [`Status`] says where the node stands, from its own state and the installer's status
//!   file ([`helper`]).
//!
//! # Trust
//!
//! The client does not decide trust and holds no release. It writes a record into a
//! directory the unprivileged service user owns, and the root installer fetches the release
//! for the commit, verifies the GitHub build attestation offline and the binary's own report,
//! and only then installs. Whatever runs as the service user can write the spool: it can
//! order an early activation of a release that verifies, or write a record the installer
//! refuses. That is a loss of availability, never an unattested install.

mod client;
#[cfg(test)]
mod fixtures;
mod floors;
pub mod helper;
mod offer;
pub mod record;
pub mod spool;
mod types;
mod wire;

pub use client::{Config, Decision, Handle, StartError, Worker, start};
pub use offer::Refusal;
pub use record::{
    Facts, Locator, MAX_AGE_SECS, MAX_RECORD_BYTES, MAX_RECORDS, OrderRecord, RecordError,
};
pub use types::{
    COMPONENT_NAMES, Component, Expected, MAX_ATTESTATION_BYTES, MAX_COMPONENT_BYTES, Offer, Order,
    Staged, State, Status, Verb,
};
