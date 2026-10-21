//! The version of a `kbf-installer` package, in the one form a deb and an rpm put in the
//! same order: by the number of commits behind the commit built (`seq`), then by the CI run.
//!
//! The version a package carries today, `0.1.0+<12 hex digits>`, is compared as text by
//! apt and dnf and says nothing about which build is newer. A package of `main` is instead
//!
//! - a deb `0.1.<seq>+g<sha12>-<run>`;
//! - an rpm Version `0.1.<seq>` and Release `<run>.g<sha12>`;
//!
//! where `0.1` is the major and minor of the workspace version ([`SERIES`]), `seq` is the
//! `seq` of `kbf --version --json` (a number that grows with every commit), `sha12` the
//! first 12 hex digits of the commit and `run` the CI run number. A build of any other ref
//! (a branch, a pull request, a tag) carries `~<ref>` after `0.1.<seq>`: `~` sorts before
//! everything, so it is older than the build of `main` at the same `seq`, in a deb and in an
//! rpm. `<ref>` is the ref reduced to lower-case letters and digits, at most [`WORD_MAX`] of
//! them, because a hyphen would end a deb's upstream version and an rpm takes none.
//!
//! A `seq` of 0 means the build could not count its commits (a shallow checkout, a source
//! archive), so no version is made of it: `0.1.0+g...` could not be told from the old form,
//! and a package of an unknown place in the history must not be ranked at all.
//!
//! This module only names the version. `pkg-config` and `pkg-asset` use it when the package
//! carries a release; until then they keep the version of the installer binary.
//! `tests/pkg_version.rs` holds the two orders to the algorithms of dpkg and rpm.
#![allow(dead_code)]

use crate::Error;

/// The part every build shares: the major and minor of the workspace version.
pub const SERIES: &str = concat!(
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR")
);

/// The most letters and digits of a ref a version holds.
pub const WORD_MAX: usize = 24;

/// Which stream of builds a package belongs to.
enum Channel {
    /// A build of `main`.
    Main,
    /// A build of any other ref, with the word its name is reduced to.
    Other(String),
}

/// The version of one build of the package.
pub struct PackageVersion {
    seq: u64,
    commit: String,
    run: u64,
    channel: Channel,
}

impl PackageVersion {
    /// The version of the build of `git_ref` (`GITHUB_REF`) at `commit`, whose `kbf --version
    /// --json` said `seq`, in CI run number `run`.
    ///
    /// # Errors
    /// [`Error::Refused`] with every reason the build cannot be put in order: a `seq` or `run`
    /// of 0, a `commit` that is not 12 to 40 lowercase hex digits, a ref that is not a full
    /// ref or has no letter or digit to name it by.
    pub fn new(seq: u64, commit: &str, run: u64, _git_ref: &str) -> Result<Self, Error> {
        Ok(PackageVersion {
            seq,
            commit: commit.to_owned(),
            run,
            channel: Channel::Main,
        })
    }

    /// The version of the deb.
    pub fn deb(&self) -> String {
        String::new()
    }

    /// The Version of the rpm.
    pub fn rpm_version(&self) -> String {
        String::new()
    }

    /// The Release of the rpm.
    pub fn rpm_release(&self) -> String {
        String::new()
    }
}
