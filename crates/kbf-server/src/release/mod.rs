//! What a release's build seq says about moving to it. The release store that lived here
//! (the tarballs of a release kept by a frontend, and `ReleaseService`) was removed: a new
//! release reaches a host as a package.
//!
//! - [`age`] is rule R4 of `docs/design/frontend-proxy.md`: a pure decision on two builds'
//!   seqs, with no store behind it.

pub mod age;
