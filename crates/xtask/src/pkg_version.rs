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
//! archive), so no version is made of it: a package of an unknown place in the history must
//! not be ranked at all, and `0.1.0+g...` would rank it, below every counted build of the series.
//!
//! This module only names the version. `pkg-config` and `pkg-asset` use it when the package
//! carries a release; until then they keep the version of the installer binary.
//! `tests/pkg_version.rs` holds the two orders to the algorithms of dpkg and rpm.

use crate::{Error, build_commit};

/// The part every build shares: the major and minor of the workspace version.
pub const SERIES: &str = concat!(
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR")
);

/// The most letters and digits of a ref a version holds.
pub const WORD_MAX: usize = 24;

/// The ref of `main`, as `GITHUB_REF` spells it.
const MAIN_REF: &str = "refs/heads/main";

/// What `GITHUB_REF` of a branch starts with.
const HEADS: &str = "refs/heads/";

const SEQ_ZERO: &str = "seq is 0: the checkout is shallow or has no git history, so the build \
                        cannot be put in order (check out with fetch-depth: 0)";

const RUN_ZERO: &str = "run number is 0: a CI run is numbered from 1";

/// Which stream of builds a package belongs to.
enum Channel {
    /// A build of `main`.
    Main,
    /// A build of any other ref, with the word its name is reduced to.
    Other(String),
}

/// The channel of the build of `git_ref` (`GITHUB_REF`), or why it has none.
fn channel_of(git_ref: &str) -> Result<Channel, String> {
    if git_ref == MAIN_REF {
        return Ok(Channel::Main);
    }
    if !git_ref.starts_with("refs/") {
        return Err(format!(
            "ref {git_ref:?} is not a full ref (GITHUB_REF: refs/heads/<branch>)"
        ));
    }
    let name = git_ref.strip_prefix(HEADS).unwrap_or(git_ref);
    let word: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .take(WORD_MAX)
        .collect();
    if word.is_empty() {
        return Err(format!(
            "ref {git_ref:?} has no letter or digit to name the build by"
        ));
    }
    Ok(Channel::Other(word))
}

/// The version of one build of the package.
pub struct PackageVersion {
    seq: u64,
    /// The first 12 hex digits of the commit.
    commit: String,
    run: u64,
    channel: Channel,
}

impl PackageVersion {
    /// The version of the build of `git_ref` (`GITHUB_REF`) at `commit`, whose `kbf --version
    /// --json` said `seq`, in CI run number `run`.
    ///
    /// `git_ref` must be the ref that was built, `GITHUB_REF` of a push or `github.ref`. Not
    /// the base branch of a pull request (`artifacts.yml` calls that `PKG_SOURCE_REF`): it is
    /// `refs/heads/main` for a pull request into `main`, and the build of a pull request
    /// would be named as a build of `main`.
    ///
    /// # Errors
    /// [`Error::Refused`] with every reason the build cannot be put in order: a `seq` or `run`
    /// of 0, a `commit` that is not 12 to 40 lowercase hex digits, a ref that is not a full
    /// ref or has no letter or digit to name it by.
    pub fn new(seq: u64, commit: &str, run: u64, git_ref: &str) -> Result<Self, Error> {
        let mut problems = Vec::new();
        if seq == 0 {
            problems.push(SEQ_ZERO.to_owned());
        }
        if !build_commit::is_commit(commit) {
            problems.push(format!("commit {commit:?} is not 12 to 40 hex digits"));
        }
        if run == 0 {
            problems.push(RUN_ZERO.to_owned());
        }
        let channel = channel_of(git_ref);
        if let Err(why) = &channel {
            problems.push(why.clone());
        }
        match channel {
            Ok(channel) if problems.is_empty() => Ok(PackageVersion {
                seq,
                commit: commit[..12].to_owned(),
                run,
                channel,
            }),
            _ => Err(Error::Refused(problems)),
        }
    }

    /// `0.1.<seq>`, and `~<ref>` after it for a build of a ref other than `main`.
    fn core(&self) -> String {
        match &self.channel {
            Channel::Main => format!("{SERIES}.{}", self.seq),
            Channel::Other(word) => format!("{SERIES}.{}~{word}", self.seq),
        }
    }

    /// The version of the deb: `0.1.<seq>+g<sha12>-<run>`.
    pub fn deb(&self) -> String {
        format!("{}+g{}-{}", self.core(), self.commit, self.run)
    }

    /// The Version of the rpm: `0.1.<seq>`.
    pub fn rpm_version(&self) -> String {
        self.core()
    }

    /// The Release of the rpm: `<run>.g<sha12>`.
    pub fn rpm_release(&self) -> String {
        format!("{}.g{}", self.run, self.commit)
    }
}
