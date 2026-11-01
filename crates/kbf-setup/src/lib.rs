//! The first configuration of a kbf host: the library behind `kbf-installer setup`.
//!
//! - [`setup`]: the verb itself: it validates its flags, then makes the host what they say
//!   (accounts, directories, `installer.toml`, `kbf.toml`, the units, the state database's
//!   schema), as a plan of `kbf-host` steps that is printed without `--apply`.
//! - [`cli`]: the flags the verb takes ([`cli::SetupArgs`]) and the `kbf` modes a host can be
//!   set up for ([`cli::Mode`]).
//! - [`layout`]: the paths ([`layout::Layout`], a directory tree and its [`layout::Platform`]).
//! - [`installer_toml`]: the installer's settings file, read closed and written whole.
//! - [`database`]: the state database the installer owns: its URL, and its schema created or
//!   brought forward.
//! - [`exit`]: the exit codes of the `kbf-installer` binary.
//! - [`sha256_hex`]: a SHA-256 digest written as text.
//! - [`CommitId`]: a commit as 40 lowercase hex characters, the only form a release is named
//!   by.
//!
//! `kbf-installer` re-exports these modules under the paths it has always had
//! (`kbf_installer::layout`, `kbf_installer::setup`, ...); this crate does not depend on it.
//!
//! Two programs run the verb: `kbf-installer setup` and `kbf setup`. The proxy's side of it
//! (the `[proxy]` table of `installer.toml`, which `setup` keeps as it is, and the proxy's flags,
//! which `kbf-installer setup` takes only to refuse) is the `proxy` feature, which
//! `kbf-installer` turns on and `kbf` does not, so the `kbf` binary links no proxy code.

pub mod cli;
pub mod database;
pub mod exit;
#[cfg(all(test, feature = "proxy"))]
mod golden;
pub mod installer_toml;
pub mod layout;
pub mod setup;
pub mod sha256_hex;

/// A commit as 40 lowercase hex characters; it lives in `kbf-retain`, which the helper's Mac
/// build takes without this crate.
pub use kbf_retain::{CommitId, NotACommit};
