//! What the installer needs from the thing that pulls a release: a [`Fetcher`] that, given
//! where the release is (the order record's locators), returns the release installed and
//! verified in its directory, or says why not. The cycle calls it for every activation of a
//! commit that is not `current` (the fetcher re-verifies a directory already on the host; a
//! directory is not proof) and changes nothing else until it returns.
//!
//! The real fetcher is [`fetcher_real::RealFetcher`](super::fetcher_real::RealFetcher): the
//! unprivileged fetch child and the verify pipeline behind this trait. The binary does not
//! build one yet; until it does it uses [`NotWired`], which refuses, so an activation of a
//! commit that is not installed is refused with that reason and nothing is fetched.

use kbf_update::{Facts, Locator};

use crate::CommitId;

/// Where the release for an order can be fetched. Locators and claims, not trust: the
/// fetcher re-derives every one that matters. Where the bytes come from is the installer's
/// own configuration, never anything in here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseLocator {
    pub commit: CommitId,
    pub components: Vec<Locator>,
}

/// A release that is installed and verified in `releases/<commit>/`, and what its own
/// `--version --json` declared. The cycle compares these with what the order expected
/// before it moves a link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledRelease {
    pub commit: CommitId,
    pub platform: String,
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
    pub profile: String,
}

/// Why a release was not installed. Nothing but scratch has changed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// The release could not be had, and nothing says it is bad: the source could not be
    /// reached or ended early, the host had no room or could not do the work, or there is
    /// no list of what to fetch.
    #[error("fetch: {0}")]
    Unavailable(String),
    /// What arrived failed a check (size, digest, attestation, shape, the binary's report).
    /// The message starts with the contract's refusal word and a colon (`hash: ...`).
    #[error("fetch: rejected: {0}")]
    Rejected(String),
    /// This build has no fetcher.
    #[error("fetch: this installer has no release fetcher wired in")]
    NotWired,
}

/// Pulls and installs a release.
pub trait Fetcher {
    /// Fetch, verify and install the release `rec` names into its directory under
    /// `releases/`. Never moves `current` or `previous`. This is the frontend's own update:
    /// the order carries no facts to compare with, so only the commit and the release
    /// profile are checked of what the binary declares.
    fn fetch(&self, rec: &ReleaseLocator) -> Result<InstalledRelease, FetchError>;

    /// [`Fetcher::fetch`] for a node's `activate`, whose order also carries the facts the
    /// release must declare about itself (`expected`): a release that declares others is not
    /// installed. A fetcher that does not check them leaves this to `fetch`; the cycle
    /// compares what comes back with the order's claims before a link moves either way.
    fn fetch_activation(
        &self,
        rec: &ReleaseLocator,
        _expected: &Facts,
    ) -> Result<InstalledRelease, FetchError> {
        self.fetch(rec)
    }
}

/// The fetcher of a build that has none: every fetch is refused.
pub struct NotWired;

impl Fetcher for NotWired {
    fn fetch(&self, _rec: &ReleaseLocator) -> Result<InstalledRelease, FetchError> {
        Err(FetchError::NotWired)
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A fetcher for tests: it records its calls, and installs the release the way the real
    //! one leaves it (a directory with a `kbf` in it) or fails as told.

    use std::fs;
    use std::sync::{Arc, Mutex};

    use kbf_update::Facts;

    use super::{FetchError, Fetcher, InstalledRelease, ReleaseLocator};
    use crate::layout::Layout;

    /// A change a test makes to the report of the release it installs.
    pub type Tamper = fn(&mut InstalledRelease);

    #[derive(Clone)]
    pub struct FakeFetcher {
        layout: Layout,
        pub calls: Arc<Mutex<Vec<ReleaseLocator>>>,
        /// The facts each `fetch_activation` was given, in order (a plain `fetch` adds none).
        pub expected: Arc<Mutex<Vec<Facts>>>,
        /// What the next fetches do; `None` installs and reports `report`.
        pub fail: Arc<Mutex<Option<FetchError>>>,
        /// Changes the report of the installed release (a binary that says other things).
        pub tamper: Arc<Mutex<Option<Tamper>>>,
    }

    impl FakeFetcher {
        pub fn new(layout: &Layout) -> Self {
            Self {
                layout: layout.clone(),
                calls: Arc::default(),
                expected: Arc::default(),
                fail: Arc::default(),
                tamper: Arc::default(),
            }
        }

        pub fn count(&self) -> usize {
            self.calls.lock().expect("lock").len()
        }
    }

    /// The report an honest release makes.
    pub fn honest(rec: &ReleaseLocator) -> InstalledRelease {
        InstalledRelease {
            commit: rec.commit.clone(),
            platform: "linux-x86_64".to_owned(),
            protocol: 7,
            min_protocol: 6,
            state_schema: 3,
            profile: "release".to_owned(),
        }
    }

    impl Fetcher for FakeFetcher {
        fn fetch(&self, rec: &ReleaseLocator) -> Result<InstalledRelease, FetchError> {
            self.calls.lock().expect("lock").push(rec.clone());
            if let Some(e) = self.fail.lock().expect("lock").take() {
                return Err(e);
            }
            let dir = self.layout.join(&self.layout.release_dir(&rec.commit));
            fs::create_dir_all(&dir).expect("release dir");
            fs::write(dir.join("kbf"), b"binary").expect("binary");
            let mut report = honest(rec);
            if self.layout.platform() == crate::layout::Platform::MacOs {
                report.platform = "darwin-arm64".to_owned();
            }
            if let Some(change) = *self.tamper.lock().expect("lock") {
                change(&mut report);
            }
            Ok(report)
        }

        fn fetch_activation(
            &self,
            rec: &ReleaseLocator,
            expected: &Facts,
        ) -> Result<InstalledRelease, FetchError> {
            self.expected.lock().expect("lock").push(expected.clone());
            self.fetch(rec)
        }
    }
}
