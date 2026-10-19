//! The frontend's release store and `ReleaseService` (`docs/design/pull-updates.md`
//! section 6): the tarballs of a release and their build attestations, kept as they were
//! uploaded and handed to hosts by digest.
//!
//! - The [`ReleaseStore`] is a directory of immutable files keyed by (commit, platform,
//!   component); see [`store`] for the layout and the write rules.
//! - The router, [`http::router`], serves `PUT /v1/releases/{commit}/{platform}/{name}` and
//!   `GET /v1/releases`.
//! - The service, [`grpc::service`], is `ReleaseService.Fetch`.
//! - The desired release, [`desired::router`]: `GET /v1/update`, `POST /v1/update/desired`,
//!   `pause` and `resume`, over the `update/desired` row.
//!
//! `GET /v1/releases` and `Fetch` carry no authentication of their own: the wiring puts `Fetch`
//! behind the worker listener's mutual TLS.
//!
//! - The offers, [`offers::Offers`]: the `UpdateOffer` each platform is to be sent over its
//!   nodes' sessions, built from the desired release and what the store holds.
//!
//! The frontend mounts the routers on its operator API and the service on its worker
//! listener where it starts. Trust is the host's: the store neither opens a tarball nor
//! checks an attestation.

pub mod desired;
pub mod grpc;
pub mod http;
pub mod key;
pub mod offers;
pub mod store;

pub use grpc::service;
pub use http::router;
pub use key::{Commit, Component, Key, KeyError, Part, Sha256Hex};
pub use store::{
    CommitListing, ComponentInfo, Limits, PlatformListing, PutError, ReleaseStore, Stored,
};
