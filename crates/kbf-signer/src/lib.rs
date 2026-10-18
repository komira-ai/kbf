//! The signing process of the farm's certificate authority
//! (`docs/design/install-and-enrollment.md`, sections 5.5 and 6.1).
//!
//! The frontend builds each certificate (`kbf_ca::Authority`) but does not hold the CA
//! key. This crate is the process that does:
//!
//! - [`SignerServer`] owns the key and serves a Unix socket. The socket is mode 0600 in a
//!   directory of mode 0700 ([`bind`]); every connection's `SO_PEERCRED` user id must be
//!   the one configured (the frontend's), and any other peer is closed on without a
//!   byte. The process gives up its dumpable flag first ([`harden`]), so no one reads the
//!   key out of its memory with ptrace or a core file. Nothing here logs, and no type
//!   prints key material.
//! - It never signs arbitrary bytes. Each request is the to-be-signed structure of one
//!   certificate, and [`check_tbs`] parses it and refuses anything but the one shape the
//!   design's table allows for the route it came in on ([`Route`]): not a CA, not a role
//!   other than the route's (the node route cannot mint a `frontend` certificate), no
//!   lifetime over the policy, only the key usage, extended key usage and extensions of
//!   the table. A frontend that is fully compromised can therefore mint only well-formed
//!   node certificates.
//! - [`SocketSigner`] is the frontend's side: a [`kbf_ca::Signer`] whose custody is
//!   [`kbf_ca::Custody::External`], so `Authority::require_external` accepts it.
//!
//! Creating the root (and so signing a CA certificate) is not a socket request: `kbf ca
//! init` does it once, in the account that will own the key.
//!
//! # Wire
//!
//! One request and one answer per connection, each a frame: a big-endian `u32` length
//! (1 to 4096) of what follows, a tag byte, then the body. The request tag is 0 (the
//! public key; empty body), 1 (sign for the node route) or 2 (sign for the frontend
//! route); the body of a signing request is the DER to-be-signed structure. The answer
//! tag is 0 (ok: the SubjectPublicKeyInfo or the DER signature), 1 (refused: the reason,
//! text) or 2 (the signer failed: text). A frame that does not parse closes the
//! connection without an answer.

mod cli;
mod client;
mod error;
mod server;
mod validate;
mod wire;

pub use cli::{Flags, main, run};
pub use client::SocketSigner;
pub use error::SignerError;
pub use server::{SignerServer, bind, harden};
pub use validate::{Refusal, Route, check_tbs};
