//! Who is calling, read from the client certificate and from nothing else.
//!
//! The reader is `kbf_updater::identity`, which the updater uses on its own certificate and
//! this crate uses on the one a caller presents: one reading, so the two cannot disagree about
//! who a certificate names. The heartbeat's body is never asked who sent it.

pub use kbf_updater::identity::{Identity, IdentityError, UPDATER_ROLE, identity_of};
