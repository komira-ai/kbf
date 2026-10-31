//! The farm's certificate authority (`docs/design/install-and-enrollment.md`, sections 5.5
//! and 6.1 to 6.4).
//!
//! What the enroll listener needs to turn a node's request into a certificate, and
//! nothing that touches a network or the state store:
//!
//! - [`Signer`]: the seam the CA key sits behind. [`FileSigner`] keeps the key in a file
//!   under a directory only its owner can enter and is for development and tests; a
//!   production frontend refuses it ([`Authority::require_external`]) and signs through a
//!   process that owns the key.
//! - [`parse_csr`]: reads a certificate request, takes **only** its public key (and the
//!   signed `issued_at` a renewal carries), and refuses everything the design refuses.
//! - [`Authority`]: builds the one certificate shape of the design's table, for a node
//!   ([`Authority::issue_node`]) or for the frontend itself ([`Authority::issue_frontend`],
//!   a separate function: the role type a token grants has no frontend variant).
//!   The update coordinator's certificate, the job scheduler's and each host updater's are
//!   separate functions too ([`Authority::issue_coordinator`],
//!   [`Authority::issue_scheduler`], [`Authority::issue_updater`]).
//! - [`Bundle`] and [`Rotation`]: the PEM bundle every trust surface reads and the
//!   staged replacement of the root, cross-signed so a node holding only the old pin
//!   still verifies.
//!
//! Time is a parameter (unix seconds) and so is the random source ([`Entropy`]); the CA
//! key itself is created from the operating system's random source, because a replay of
//! a private key is never wanted. Nothing here logs, and no type prints key material.

mod authority;
mod bundle;
mod csr;
mod der;
mod entropy;
mod error;
mod key;
mod keyfile;
mod policy;
mod serial;
mod signer;
pub mod verify_cert;

pub use authority::{Authority, Issued, NodeCert, Rotation, UpdaterCert};
pub use bundle::Bundle;
pub use csr::{ISSUED_AT_OID, MAX_CSR_LEN, ParsedCsr, parse_csr};
pub use entropy::{Entropy, SystemEntropy};
pub use error::{CaError, CsrError, KeyFileFault};
pub use key::{KeyError, PublicKey};
pub use policy::{CLOCK_FLOOR_UNIX, Policy};
pub use signer::{Algorithm, Custody, FileSigner, SignError, Signer};
