//! What the installer needs from the thing that pulls a release: a [`Fetcher`] that, given
//! where the release is (the order record's locators), returns the release installed and
//! verified in its directory, or says why not. The cycle calls it for every activation of a
//! commit that is not `current` (the fetcher re-verifies a directory already on the host; a
//! directory is not proof) and changes nothing else until it returns. A frontend's
//! `frontend_update` calls it too, with no components: the source is the installer's own.
//!
//! The real fetcher is [`fetcher_real::RealFetcher`](super::fetcher_real::RealFetcher): the
//! unprivileged fetch child and the verify pipeline behind this trait, which the binary has
//! built for each fetch by [`fetcher_host::HostFetcher`](super::fetcher_host::HostFetcher).
//! The other is [`fetcher_seed::SeedFetcher`](super::fetcher_seed::SeedFetcher): it brings in
//! no bytes, it hands the release the installer's package carries to the same verify
//! pipeline. The locator says which one an order wants ([`ReleaseLocator::source`]), and the
//! host fetcher decides whether the host's configuration allows it. [`NotWired`] is the
//! fetcher of a build that has none: it refuses, and nothing is fetched.

use kbf_update::{Facts, Locator, Source};

use crate::CommitId;
use crate::binary::Version;
use crate::seed::{Looked, Noticed, SeedFacts};

/// Where the release for an order can be fetched. Locators and claims, not trust: the
/// fetcher re-derives every one that matters. Where the bytes come from is the installer's
/// own configuration, never anything in here: [`source`](ReleaseLocator::source) only chooses
/// between that configuration and the release the package carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseLocator {
    pub commit: CommitId,
    pub components: Vec<Locator>,
    /// Whether to fetch the release from the configured source or to use the one the package
    /// carries (which has no components to name: its manifest is the locator).
    pub source: Source,
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
    /// reached or ended early, said it will not describe the release or described it
    /// unusably, or the host had no room or could not do the work.
    #[error("fetch: {0}")]
    Unavailable(String),
    /// What arrived failed a check (size, digest, attestation, shape, the binary's report).
    /// The message starts with the contract's refusal word and a colon (`hash: ...`).
    #[error("fetch: rejected: {0}")]
    Rejected(String),
    /// This build has no fetcher.
    #[error("fetch: this installer has no release fetcher wired in")]
    NotWired,
    /// The order itself is refused, before a byte is fetched or read: this host's
    /// configuration does not allow its source, or it names a release this host does not
    /// stage. `word` is the contract's refusal word it is reported under.
    #[error("{word}: {why}")]
    Order { word: &'static str, why: String },
}

/// Pulls and installs a release.
///
/// Both forms are required, so that no implementor (a decorator, a test double, a merge that
/// adds one) can answer an activation in the form that checks none of the order's facts:
///
/// ```compile_fail,E0046
/// use kbf_installer::helper::fetcher::{FetchError, Fetcher, InstalledRelease, ReleaseLocator};
///
/// struct OnlyFetch;
///
/// impl Fetcher for OnlyFetch {
///     fn fetch(&self, _: &ReleaseLocator) -> Result<InstalledRelease, FetchError> {
///         Err(FetchError::NotWired)
///     }
/// }
/// ```
pub trait Fetcher {
    /// Fetch, verify and install the release `rec` names into its directory under
    /// `releases/`. Never moves `current` or `previous`, and never writes the state: the
    /// caller that keeps the release records it (`staged` for a frontend update, `current`
    /// for an activation), so a release it then refuses leaves nothing behind in the state.
    /// This is the frontend's own update: the order carries no facts to compare with, so
    /// only the commit and the release profile are checked of what the binary declares.
    fn fetch(&self, rec: &ReleaseLocator) -> Result<InstalledRelease, FetchError>;

    /// [`Fetcher::fetch`] for a node's `activate`, whose order also carries the facts the
    /// release must declare about itself (`expected`): a release that declares others is not
    /// installed. The cycle compares what comes back with the order's claims before a link
    /// moves either way, but a fetcher that ignores `expected` would install a release the
    /// order never described, so an implementor says what it does with them.
    fn fetch_activation(
        &self,
        rec: &ReleaseLocator,
        expected: &Facts,
    ) -> Result<InstalledRelease, FetchError>;

    /// What the release the host's package carries is, by the package's own manifest: its
    /// commit, its `seq` and the facts it declares. An order whose source is
    /// [`Source::Staged`] carries no facts, so the cycle takes them from here and passes them
    /// to [`Fetcher::fetch_activation`], which holds the release to them; `setup` asks for the
    /// commit it has no `--commit` to name. It reads the manifest and decides nothing about an
    /// order. A fetcher that has no package says so.
    fn seed(&self) -> Result<SeedFacts, FetchError> {
        Err(FetchError::NotWired)
    }

    /// What the run loop learns at the end of each pass (not again for a minute after a look
    /// that failed): the source `installer.toml` names, and whether the package's seed is whole
    /// (see [`Noticed`]). `known` is the seed
    /// found whole by an earlier look: while the package's manifest is the one it was found by,
    /// the package is not checked again, and `known` is the answer. It installs nothing, moves
    /// nothing and decides nothing about an order. A fetcher that has no package says so.
    fn look(&self, _known: Option<&Looked>) -> Noticed {
        Noticed {
            source: None,
            seed: Err(FetchError::NotWired),
        }
    }

    /// The word `release_source` holds in `installer.toml` (`url`, `frontend` or `bundled`), as
    /// of now; `None` when the file cannot be read or the host cannot say. The operator's
    /// `rollback` asks it, to hold a host that fetches nothing to a kbf that relays.
    fn source(&self) -> Option<&'static str> {
        None
    }

    /// What the installed release `commit` declares of itself (`kbf --version --json`), asked
    /// of its own binary as the unprivileged user the verify pipeline runs a binary as. The
    /// release is one this host installed and runs: the question is what it says, not whether
    /// it may run. A fetcher that cannot run a binary says so.
    fn probe(&self, _commit: &CommitId) -> Result<Version, FetchError> {
        Err(FetchError::NotWired)
    }
}

/// The fetcher of a build that has none: every fetch is refused.
pub struct NotWired;

impl Fetcher for NotWired {
    fn fetch(&self, _rec: &ReleaseLocator) -> Result<InstalledRelease, FetchError> {
        Err(FetchError::NotWired)
    }

    fn fetch_activation(
        &self,
        _rec: &ReleaseLocator,
        _expected: &Facts,
    ) -> Result<InstalledRelease, FetchError> {
        Err(FetchError::NotWired)
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A fetcher for tests: it records its calls, and installs the release the way the real
    //! one leaves it (a directory with a `kbf` in it) or fails as told.

    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::{Arc, Mutex};

    use kbf_update::Facts;

    use super::{FetchError, Fetcher, InstalledRelease, ReleaseLocator};
    use crate::CommitId;
    use crate::binary::Version;
    use crate::layout::Layout;
    use crate::seed::{Looked, Noticed, SeedFacts};

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
        /// The release this fetcher's package carries; `None` for a host with no package.
        pub seed: Arc<Mutex<Option<SeedFacts>>>,
        /// What the package carries once the next fetch has installed (`Some(None)`: nothing
        /// that can be read), a package swapped while its release was fetched.
        pub swap_seed: Arc<Mutex<Option<Option<SeedFacts>>>>,
        /// The word `installer.toml` is said to hold for the source, if any.
        pub source: Arc<Mutex<Option<&'static str>>>,
        /// What each installed release prints for `--version --json`, by commit; a commit with
        /// no entry cannot be asked.
        pub versions: Arc<Mutex<BTreeMap<CommitId, String>>>,
        /// The commits `probe` was asked about and had something to say of (a commit in
        /// `versions`), in order.
        pub probes: Arc<Mutex<Vec<CommitId>>>,
    }

    impl FakeFetcher {
        pub fn new(layout: &Layout) -> Self {
            Self {
                layout: layout.clone(),
                calls: Arc::default(),
                expected: Arc::default(),
                fail: Arc::default(),
                tamper: Arc::default(),
                seed: Arc::default(),
                swap_seed: Arc::default(),
                source: Arc::default(),
                versions: Arc::default(),
                probes: Arc::default(),
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
            if let Some(next) = self.swap_seed.lock().expect("lock").take() {
                *self.seed.lock().expect("lock") = next;
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

        fn seed(&self) -> Result<SeedFacts, FetchError> {
            let carried = self.seed.lock().expect("lock").clone();
            carried.ok_or(FetchError::NotWired)
        }

        fn look(&self, _known: Option<&Looked>) -> Noticed {
            let carried = self.seed.lock().expect("lock").clone();
            let seed = carried.map_or(Err(FetchError::NotWired), |seed| {
                Ok(Some(Looked {
                    digest: "fake".to_owned(),
                    seed,
                }))
            });
            Noticed {
                source: *self.source.lock().expect("lock"),
                seed,
            }
        }

        fn source(&self) -> Option<&'static str> {
            *self.source.lock().expect("lock")
        }

        fn probe(&self, commit: &CommitId) -> Result<Version, FetchError> {
            let said = self.versions.lock().expect("lock").get(commit).cloned();
            let said = said.ok_or(FetchError::NotWired)?;
            self.probes.lock().expect("lock").push(commit.clone());
            Version::parse(said.as_bytes()).map_err(|e| FetchError::Unavailable(e.to_string()))
        }
    }
}
