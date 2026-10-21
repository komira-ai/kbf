//! The fetcher of the release the package carries (see [`crate::seed`]): it brings in no
//! bytes from anywhere, it copies the seed's tarball into the scratch directory a fetch would
//! have filled and hands it to the verify pipeline, which checks it exactly as it checks a
//! fetched one. Nothing here decides anything the pipeline decides: the digest and size the
//! pipeline holds the tarball to are the manifest's, the attestation is the bundle in the seed
//! directory, and the facts the binary must declare are the manifest's.
//!
//! What this fetcher adds to the pipeline's checks, before the pipeline runs, is consistency
//! of the package: the seed directory is root's and so are the files in it
//! ([`trusted_chain`]), the manifest names the commit this installer was built from, the order
//! names the manifest's commit (all 40 digits), the bundle is the one the manifest's digest
//! names, and the facts an order expects are the manifest's. After it, the `seq` the binary
//! declares must be the manifest's. A package that fails any of them is a package that is
//! missing a file, was built from two builds, or was swapped; none of it can make a release
//! pass the pipeline that would not.
//!
//! Whether a host may use the seed at all (the floor on `seq`, the order's source) is not this
//! fetcher's: the cycle checks the first before it asks, and
//! [`HostFetcher`](super::fetcher_host::HostFetcher) decides the second.

use kbf_host::exec::Exec;
use kbf_host::step::Owner;
use kbf_update::Facts;

use crate::helper::fetcher::{FetchError, Fetcher, InstalledRelease, ReleaseLocator};
use crate::helper::fetcher_real::Parts;
use crate::helper::leases::LeaseSource;
use crate::layout::Layout;
use crate::run::Runner;
use crate::seed::SeedFacts;
use crate::settings::Settings;
use crate::stage::Policy;

/// Verifies and installs the release the package carries, as of the moment it is asked.
pub struct SeedFetcher {
    pub host: Layout,
    pub settings: Settings,
    pub policy: Policy,
    /// Says who owns the seed and the trust list and reads the small files; the host's own on
    /// a host.
    pub trust: Box<dyn Exec>,
    pub runner: Box<dyn Runner>,
    /// Observed at the pipeline's lease step, for an activation.
    pub leases: Box<dyn LeaseSource>,
    /// Who owns the scratch directory and the release: root on a host.
    pub owner: Owner,
    /// The first 12 hex digits of the commit this installer was built from, which name its
    /// seed directory.
    pub build: String,
}

impl SeedFetcher {
    /// A fetcher for the host `host` with `settings` and `policy`, taking from `parts` what
    /// the host supplies (the fetch child, which a seed never starts, is dropped), reading the
    /// seed directory of the installer built from `build`.
    pub fn new(
        host: Layout,
        settings: Settings,
        policy: Policy,
        parts: Parts,
        build: &str,
    ) -> SeedFetcher {
        SeedFetcher {
            host,
            settings,
            policy,
            trust: parts.trust,
            runner: parts.runner,
            leases: parts.leases,
            owner: parts.owner,
            build: build.to_owned(),
        }
    }
}

impl Fetcher for SeedFetcher {
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

    fn seed(&self) -> Result<SeedFacts, FetchError> {
        Err(FetchError::NotWired)
    }
}

#[cfg(test)]
#[path = "fetcher_seed_tests.rs"]
mod tests;
